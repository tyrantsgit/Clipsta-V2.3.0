//! Clipsta Tauri v2 — main application setup
//!
//! - All commands registered
//! - System tray with context menu
//! - Global shortcuts for clip saves and recording toggle
//! - In-process WGC + WASAPI capture (no separate process)

pub mod audio;
pub mod commands;
pub mod gpu_capture;
pub mod lossless_trim;
pub mod mp4_inspect;
pub mod settings;
pub mod watch_folder;


use std::sync::atomic::Ordering;

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

use gpu_capture::CaptureSession;
use settings::SettingsStore;
use watch_folder::WatchFolderService;

/// Register global hotkeys based on current settings.
pub fn register_hotkeys(app: &tauri::AppHandle, settings: &settings::AppSettings) {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();

    let app_clone = app.clone();
    let try_register = |accel: &str, seconds: u32| {
        if accel.is_empty() {
            return;
        }
        if let Ok(shortcut) = accel.parse::<Shortcut>() {
            let app_h = app_clone.clone();
            let _ = gs.on_shortcut(shortcut, move |_app, _shortcut, _event| {
                let _ = app_h.emit("hotkey:clip", seconds);
            });
        }
    };

    try_register(&settings.hotkey_clip30_sec, 30);
    try_register(&settings.hotkey_clip1_min, 60);
    try_register(&settings.hotkey_clip5_min, 300);

    // Record toggle
    if !settings.hotkey_record.is_empty() {
        if let Ok(shortcut) = settings.hotkey_record.parse::<Shortcut>() {
            let app_h = app_clone.clone();
            let _ = gs.on_shortcut(shortcut, move |_app, _shortcut, _event| {
                let _ = app_h.emit("hotkey:record", ());
            });
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // NOTE: Do NOT call CoInitializeEx here. Tauri's window library (tao) requires
    // OleInitialize (STA) on the main thread. COM MTA is initialized on capture/audio
    // threads instead (see capture.rs and audio.rs).

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            // Settings
            commands::settings_get_all,
            commands::settings_set,
            commands::settings_set_all,
            // Clips
            commands::clips_list,
            commands::clips_delete,
            commands::clips_rename,
            commands::clips_import,
            commands::clips_import_folder,
            // Recording
            commands::wgc_sources,
            commands::wgc_start_recording,
            commands::wgc_stop_recording,
            commands::wgc_save_clip,
            commands::wgc_save_full_recording,
            // Export
            commands::recording_export,
            commands::compress_for_upload,
            // File ops
            commands::shell_open_folder,
            commands::shell_open_file,
            commands::shell_show_item,
            commands::file_stat,
            commands::file_ensure_dir,
            commands::file_copy_to_downloads,
            // Audio
            commands::audio_list_devices,
            commands::audio_default_devices,
            // System
            commands::system_info,
            commands::get_active_window_title,
            // Hotkeys
            commands::hotkeys_suspend,
            commands::hotkeys_resume,
            // MP4 Inspection
            commands::mp4_inspect,
            commands::mp4_keyframes,
            // Lossless Trim
            commands::lossless_trim_clip,
            // Watch Folder
            commands::watch_folder_start,
            commands::watch_folder_stop,
            commands::watch_folder_status,
        ])
        .setup(|app| {
            // Load settings (non-fatal: use defaults if file is corrupted)
            let app_data = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            let _ = std::fs::create_dir_all(&app_data);
            let store = SettingsStore::load(&app_data);

            // Ensure output folder exists
            let settings = store.get();
            if !settings.output_folder.is_empty() {
                let _ = std::fs::create_dir_all(&settings.output_folder);
            }

            // Create capture session
            let session = CaptureSession::new();

            // Clean up old temp recording directories and orphaned temp files from previous sessions
            let temp_dir = std::env::temp_dir();
            if let Ok(entries) = std::fs::read_dir(&temp_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    // Remove orphaned segment directories
                    if name.starts_with("clipsta_segments_") || name == "clipsta_recording" {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                    // Remove orphaned clip temp files (from crashed saves)
                    if name.starts_with("clipsta_clip_video_") || name.starts_with("clipsta_concat_") {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }

            // Manage state
            app.manage(store.clone());
            app.manage(session);

            // Create and manage watch folder service
            let watch_service = WatchFolderService::new();
            app.manage(watch_service.clone());

            // Auto-start watch folder if enabled in settings
            if settings.watch_folder_enabled && !settings.watch_folder_path.is_empty() {
                let app_handle = app.handle().clone();
                let path = settings.watch_folder_path.clone();
                let svc = watch_service.clone();
                // Start on a background task (needs tokio runtime)
                tokio::spawn(async move {
                    if let Err(e) = svc.start(path.clone(), app_handle) {
                        eprintln!("[watch_folder] auto-start failed: {}", e);
                    }
                });
            }

            // Setup tray icon (non-fatal if it fails)
            if let Err(e) = setup_tray(app) {
                eprintln!("[clipsta] tray setup failed (non-fatal): {}", e);
            }

            // Register global shortcuts (non-fatal: app works without them)
            let app_handle = app.handle().clone();
            register_hotkeys(&app_handle, &settings);

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let store = app.state::<SettingsStore>();
                if store.get().minimize_to_tray {
                    // Hide to tray instead of closing
                    api.prevent_close();
                    let _ = window.hide();
                } else {
                    // Actually quit the app
                    let session = app.state::<CaptureSession>();
                    if session.is_recording.load(Ordering::Relaxed) {
                        session.stop();
                    }
                    // Clean up temp recording files
                    session.cleanup();
                    app.exit(0);
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn setup_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let open_item = MenuItemBuilder::with_id("open", "Open Clipsta").build(app)?;
    let clip30_item = MenuItemBuilder::with_id("clip30", "⚡ Save Last 30 Seconds").build(app)?;
    let clip1m_item = MenuItemBuilder::with_id("clip1m", "⚡ Save Last 1 Minute").build(app)?;
    let clip5m_item = MenuItemBuilder::with_id("clip5m", "⚡ Save Last 5 Minutes").build(app)?;
    let folder_item = MenuItemBuilder::with_id("folder", "Open Clips Folder").build(app)?;
    let quit_item = MenuItemBuilder::with_id("quit", "Quit Clipsta").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&open_item)
        .separator()
        .item(&clip30_item)
        .item(&clip1m_item)
        .item(&clip5m_item)
        .separator()
        .item(&folder_item)
        .separator()
        .item(&quit_item)
        .build()?;

    let _tray = TrayIconBuilder::new()
        .tooltip("Clipsta")
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            "clip30" => {
                let _ = app.emit("hotkey:clip", 30u32);
            }
            "clip1m" => {
                let _ = app.emit("hotkey:clip", 60u32);
            }
            "clip5m" => {
                let _ = app.emit("hotkey:clip", 300u32);
            }
            "folder" => {
                let store = app.state::<SettingsStore>();
                let folder = store.get().output_folder;
                if !folder.is_empty() {
                    let _ = std::process::Command::new("explorer").arg(&folder).spawn();
                }
            }
            "quit" => {
                // Stop recording if active
                let session = app.state::<CaptureSession>();
                if session.is_recording.load(Ordering::Relaxed) {
                    session.stop();
                }
                session.cleanup();
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                let app = tray.app_handle();
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
        })
        .build(app)?;

    Ok(())
}
