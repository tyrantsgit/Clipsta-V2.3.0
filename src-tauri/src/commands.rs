//! Tauri commands — all IPC handlers for the frontend.

use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

use crate::gpu_capture::{CaptureOptions, CaptureSession, CompletedSegment, SourceInfo};
use crate::settings::{AppSettings, SettingsStore};

/// Initialize COM MTA on the current thread (for async command threads).
/// Safe to call multiple times — returns S_FALSE if already initialized.
fn ensure_com() {
    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

// ── Settings commands ─────────────────────────────────────────────────────────

#[tauri::command]
pub async fn settings_get_all(store: State<'_, SettingsStore>) -> Result<AppSettings, String> {
    Ok(store.get())
}

#[tauri::command]
pub async fn settings_set(
    app: AppHandle,
    store: State<'_, SettingsStore>,
    key: String,
    value: serde_json::Value,
) -> Result<bool, String> {
    store.set_field(&key, value);
    // Re-register hotkeys if a hotkey field was changed
    if key.starts_with("hotkey") {
        crate::register_hotkeys(&app, &store.get());
    }
    Ok(true)
}

#[tauri::command]
pub async fn settings_set_all(
    app: AppHandle,
    store: State<'_, SettingsStore>,
    settings: serde_json::Value,
) -> Result<bool, String> {
    store.set_all(settings);
    // Re-register global hotkeys with the updated settings
    crate::register_hotkeys(&app, &store.get());
    Ok(true)
}

// ── Clip management commands ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipFile {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub created_at: String,
}

#[tauri::command]
pub async fn clips_list(store: State<'_, SettingsStore>) -> Result<Vec<ClipFile>, String> {
    let settings = store.get();
    let folder = PathBuf::from(&settings.output_folder);
    if !folder.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(&folder).map_err(|e| e.to_string())?;
    let mut clips = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !matches!(ext.as_str(), "mp4" | "webm" | "mkv" | "mov") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            let created = meta
                .created()
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Local> = t.into();
                    dt.to_rfc3339()
                })
                .unwrap_or_default();
            clips.push(ClipFile {
                name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                path: path.to_string_lossy().to_string(),
                size: meta.len(),
                created_at: created,
            });
        }
    }
    clips.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(clips)
}

#[tauri::command]
pub async fn clips_delete(path: String) -> Result<bool, String> {
    if std::path::Path::new(&path).exists() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    Ok(true)
}

#[tauri::command]
pub async fn clips_rename(old_path: String, new_name: String) -> Result<String, String> {
    let dir = std::path::Path::new(&old_path)
        .parent()
        .ok_or("No parent dir")?;
    let new_path = dir.join(&new_name);
    std::fs::rename(&old_path, &new_path).map_err(|e| e.to_string())?;
    Ok(new_path.to_string_lossy().to_string())
}

#[tauri::command]
pub async fn clips_import(
    source_path: String,
    store: State<'_, SettingsStore>,
) -> Result<String, String> {
    let folder = ensure_output_folder(&store);
    let name = std::path::Path::new(&source_path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let dest = unique_path(&folder, &name);
    std::fs::copy(&source_path, &dest).map_err(|e| e.to_string())?;
    Ok(dest.to_string_lossy().to_string())
}

#[tauri::command]
pub async fn clips_import_folder(
    source_folder: String,
    store: State<'_, SettingsStore>,
) -> Result<Vec<String>, String> {
    let folder = ensure_output_folder(&store);
    let entries = std::fs::read_dir(&source_folder).map_err(|e| e.to_string())?;
    let mut imported = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !matches!(ext.as_str(), "mp4" | "webm" | "mkv" | "mov") {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let dest = unique_path(&folder, &name);
        if std::fs::copy(&path, &dest).is_ok() {
            imported.push(dest.to_string_lossy().to_string());
        }
    }
    Ok(imported)
}


// ── Recording control commands ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartRecordingOpts {
    pub source_id: Option<String>,
    pub fps: Option<u32>,
    pub no_audio: Option<bool>,
    pub mic_device: Option<String>,
    pub loopback_device: Option<String>,
}

#[tauri::command]
pub async fn wgc_sources() -> Result<Vec<SourceInfo>, String> {
    ensure_com();
    Ok(crate::gpu_capture::list_sources())
}

#[tauri::command]
pub async fn wgc_start_recording(
    app: AppHandle,
    session: State<'_, CaptureSession>,
    store: State<'_, SettingsStore>,
    opts: StartRecordingOpts,
) -> Result<serde_json::Value, String> {
    ensure_com();
    let settings = store.get();
    let fps = opts.fps.unwrap_or(settings.fps);
    let no_audio = opts.no_audio.unwrap_or(!settings.capture_audio);

    let target_w = None; // GPU capture operates at native screen resolution
    let target_h = None; // Scaling happens at export time if needed

    let bitrate = resolve_game_bar_bitrate(&settings.resolution, fps);

    let seg_dir = std::env::temp_dir().join("clipsta_recording");

    // Clean up any old recording files from previous sessions
    if seg_dir.exists() {
        let _ = std::fs::remove_dir_all(&seg_dir);
    }

    let capture_opts = CaptureOptions {
        source_id: opts.source_id,
        fps,
        no_audio,
        mic_device: opts.mic_device,
        loopback_device: opts.loopback_device,
        target_width: target_w,
        target_height: target_h,
        bitrate_kbps: bitrate,
        segment_duration: 3, // 3s segments — first clip available after 3 seconds
        buffer_duration: settings.buffer_duration,
        segment_dir: seg_dir,
    };

    let app_handle = app.clone();
    let on_segment = Box::new(move |seg: CompletedSegment| {
        let _ = app_handle.emit("wgc:segment", &seg);
    });

    eprintln!("[wgc_start_recording] calling session.start with fps={} bitrate={} no_audio={}", fps, bitrate, no_audio);
    let info = session.start(capture_opts, on_segment).map_err(|e| {
        let msg = format!("Capture start failed: {}", e);
        eprintln!("[wgc_start_recording] {}", msg);
        let _ = app.emit("wgc:error", &msg);
        msg
    })?;
    eprintln!("[wgc_start_recording] success: {}x{} @ {}fps", info.width, info.height, info.fps);

    Ok(serde_json::json!({
        "width": info.width,
        "height": info.height,
        "fps": info.fps,
        "segmentDir": info.segment_dir,
    }))
}

#[tauri::command]
pub async fn wgc_stop_recording(session: State<'_, CaptureSession>) -> Result<(), String> {
    session.stop();
    Ok(())
}

#[tauri::command]
pub async fn wgc_save_clip(
    app: AppHandle,
    session: State<'_, CaptureSession>,
    store: State<'_, SettingsStore>,
    seconds: u32,
    file_name: String,
) -> Result<Option<String>, String> {
    if session.is_saving.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("Another save is in progress".to_string());
    }

    if !session.is_recording.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(None);
    }

    let output_folder = ensure_output_folder(&store);
    // ShadowPlay-style: save clips in a game-specific subfolder
    let game_name = file_name
        .split(|c: char| c.is_ascii_digit())
        .next()
        .unwrap_or("Desktop")
        .trim()
        .to_string();
    let game_folder = if game_name.is_empty() {
        output_folder.clone()
    } else {
        let gf = output_folder.join(&game_name);
        let _ = std::fs::create_dir_all(&gf);
        gf
    };
    let output_path = game_folder.join(&file_name);
    let output_str = output_path.to_string_lossy().to_string();

    eprintln!("[wgc_save_clip] saving {}s clip to: {}", seconds, output_str);

    // Use the new persistent-encoder pipeline: keyframe-aligned slice from
    // the in-memory encoded ring → MF Sink Writer → MP4.
    // This is instant (no transcoding) because the ring already contains
    // encoded H.264 frames ready for passthrough muxing.
    match session.save_clip(seconds, &output_str) {
        Ok(path) => {
            let _ = app.emit("wgc:clipSaved", &path);
            let settings = store.get();
            if settings.clip_sound_enabled {
                let _ = app.emit("play-clip-sound", ());
            }
            eprintln!("[wgc_save_clip] clip saved: {}", path);
            Ok(Some(path))
        }
        Err(e) => {
            let msg = format!("{}", e);
            eprintln!("[wgc_save_clip] save failed: {}", msg);
            // If the ring doesn't have enough data yet, return None (not an error)
            if msg.contains("Not enough") || msg.contains("No keyframe") {
                Ok(None)
            } else {
                Err(msg)
            }
        }
    }
}

#[tauri::command]
pub async fn wgc_save_full_recording(
    app: AppHandle,
    session: State<'_, CaptureSession>,
    store: State<'_, SettingsStore>,
) -> Result<Option<String>, String> {
    // Save the entire buffer content (up to buffer_duration seconds)
    let settings = store.get();
    let output_folder = ensure_output_folder(&store);
    let stamp = chrono::Local::now().format("%Y.%m.%d - %H.%M.%S.00");
    let output_path = output_folder.join(format!("Desktop {}.DVR.mp4", stamp));
    let output_str = output_path.to_string_lossy().to_string();

    // Save the maximum buffer duration
    match session.save_clip(settings.buffer_duration, &output_str) {
        Ok(path) => {
            let _ = app.emit("wgc:clipSaved", &path);
            Ok(Some(path))
        }
        Err(e) => {
            let msg = format!("{}", e);
            if msg.contains("Not enough") || msg.contains("No keyframe") {
                Ok(None)
            } else {
                Err(msg)
            }
        }
    }
}

// ── File operation commands ───────────────────────────────────────────────────

#[tauri::command]
pub async fn shell_open_folder(path: String) -> Result<(), String> {
    Command::new("explorer")
        .arg(&path)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn shell_open_file(path: String) -> Result<(), String> {
    Command::new("explorer")
        .arg(&path)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn shell_show_item(path: String) -> Result<(), String> {
    Command::new("explorer")
        .args(["/select,", &path])
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn file_stat(file_path: String) -> Result<serde_json::Value, String> {
    let meta = std::fs::metadata(&file_path).map_err(|e| e.to_string())?;
    let modified = meta
        .modified()
        .map(|t| {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_default();
    Ok(serde_json::json!({
        "size": meta.len(),
        "modifiedAt": modified,
    }))
}

#[tauri::command]
pub async fn file_ensure_dir(dir_path: String) -> Result<bool, String> {
    std::fs::create_dir_all(&dir_path).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
pub async fn file_copy_to_downloads(file_path: String) -> Result<String, String> {
    let downloads = dirs::download_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")));
    let name = std::path::Path::new(&file_path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let dest = unique_path(&downloads, &name);
    std::fs::copy(&file_path, &dest).map_err(|e| e.to_string())?;
    Ok(dest.to_string_lossy().to_string())
}


// ── Export command ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportOpts {
    pub format: Option<String>,
    pub aspect_ratio: Option<String>,
    pub resolution: Option<String>,
    pub encoder: Option<String>,
    pub fps: Option<u32>,
    pub trim_start: Option<f64>,
    pub trim_end: Option<f64>,
    pub cuts: Option<Vec<CutRange>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CutRange {
    pub start: f64,
    pub end: f64,
}

/// Compress a clip to 720p for faster upload. Returns the path to the compressed file.
#[tauri::command]
pub async fn compress_for_upload(file_path: String) -> Result<Option<String>, String> {
    let ffmpeg = find_ffmpeg();
    let input = std::path::Path::new(&file_path);
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    // Put compressed file in TEMP dir (not next to clip — avoids showing as duplicate in Library)
    let output_path = std::env::temp_dir().join(format!("{}_upload.mp4", stem));
    let output_str = output_path.to_string_lossy().to_string();

    let args = vec![
        "-i", &file_path,
        "-vf", "scale=-2:720",
        "-c:v", "h264_nvenc",
        "-preset", "p1",
        "-rc", "constqp",
        "-qp", "28",
        "-c:a", "aac",
        "-b:a", "128k",
        "-movflags", "+faststart",
        "-y", &output_str,
    ];

    let result = {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            tokio::process::Command::new(&ffmpeg)
                .args(&args)
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .await
                .map_err(|e| format!("FFmpeg failed: {}", e))?
        }
        #[cfg(not(windows))]
        {
            tokio::process::Command::new(&ffmpeg)
                .args(&args)
                .output()
                .await
                .map_err(|e| format!("FFmpeg failed: {}", e))?
        }
    };

    if result.status.success() && output_path.exists() {
        Ok(Some(output_str))
    } else {
        // Cleanup failed attempt
        let _ = std::fs::remove_file(&output_path);
        Ok(None)
    }
}

#[tauri::command]
pub async fn recording_export(
    input_path: String,
    output_path: String,
    opts: ExportOpts,
) -> Result<String, String> {
    let ffmpeg = find_ffmpeg();
    let mut args: Vec<String> = Vec::new();

    args.extend(["-hwaccel".into(), "auto".into(), "-i".into(), input_path]);

    if let Some(start) = opts.trim_start {
        args.extend(["-ss".into(), format!("{}", start)]);
    }
    if let Some(end) = opts.trim_end {
        args.extend(["-to".into(), format!("{}", end)]);
    }

    let mut vf_filters: Vec<String> = Vec::new();

    // Cuts
    if let Some(ref cuts) = opts.cuts {
        let terms: Vec<String> = cuts
            .iter()
            .filter(|c| c.start < c.end)
            .map(|c| format!("between(t,{},{})", c.start, c.end))
            .collect();
        if !terms.is_empty() {
            vf_filters.push(format!("select='not({})',setpts=N/FRAME_RATE/TB", terms.join("+")));
        }
    }

    // Aspect ratio crop
    if let Some(ref ar) = opts.aspect_ratio {
        match ar.as_str() {
            "9:16" => vf_filters.push("crop=min(iw\\,ih*9/16):ih".into()),
            "4:3" => vf_filters.push("crop=min(iw\\,ih*4/3):ih".into()),
            _ => {}
        }
    }

    // Resolution scale
    if let Some(ref res) = opts.resolution {
        if let Some((_, h)) = resolve_target_res(res) {
            let is_portrait = opts.aspect_ratio.as_deref() == Some("9:16");
            if is_portrait {
                vf_filters.push(format!("scale={}:-2", h));
            } else {
                vf_filters.push(format!("scale=-2:{}", h));
            }
        }
    }

    if !vf_filters.is_empty() {
        args.extend(["-vf".into(), vf_filters.join(",")]);
    }

    // Audio filter for cuts
    if let Some(ref cuts) = opts.cuts {
        let a_terms: Vec<String> = cuts
            .iter()
            .filter(|c| c.start < c.end)
            .map(|c| format!("between(t,{},{})", c.start, c.end))
            .collect();
        if !a_terms.is_empty() {
            args.extend([
                "-af".into(),
                format!("aselect='not({})',asetpts=N/SR/TB", a_terms.join("+")),
            ]);
        }
    }

    // Encoder selection
    let (codec, extra) = get_encoder_args(opts.encoder.as_deref(), opts.fps.unwrap_or(60));
    args.extend(["-c:v".into(), codec]);
    args.extend(extra);

    if let Some(fps) = opts.fps {
        args.extend(["-r".into(), format!("{}", fps)]);
    }

    args.extend([
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "192k".into(),
        "-movflags".into(),
        "+faststart".into(),
        "-y".into(),
        output_path.clone(),
    ]);

    let output = {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            tokio::process::Command::new(&ffmpeg)
                .args(&args)
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .await
                .map_err(|e| format!("FFmpeg failed to start: {}", e))?
        }
        #[cfg(not(windows))]
        {
            tokio::process::Command::new(&ffmpeg)
                .args(&args)
                .output()
                .await
                .map_err(|e| format!("FFmpeg failed to start: {}", e))?
        }
    };

    if output.status.success() {
        Ok(output_path)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().rev().take(3).collect::<Vec<_>>().join(" ");
        Err(format!("FFmpeg error: {}", detail))
    }
}

// ── Audio device listing ──────────────────────────────────────────────────────

#[tauri::command]
pub async fn audio_list_devices() -> Result<Vec<serde_json::Value>, String> {
    ensure_com();
    crate::audio::WasapiCapture::list_audio_devices().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn audio_default_devices() -> Result<serde_json::Value, String> {
    ensure_com();
    crate::audio::WasapiCapture::get_default_devices().map_err(|e| e.to_string())
}

// ── System info ───────────────────────────────────────────────────────────────

/// Returns the title of the currently focused foreground window.
/// Used for ShadowPlay-style clip naming (e.g., "Battlefield 6").
#[tauri::command]
pub async fn get_active_window_title() -> Result<String, String> {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return Ok("Desktop".to_string());
        }
        let mut buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len == 0 {
            return Ok("Desktop".to_string());
        }
        let title = String::from_utf16_lossy(&buf[..len as usize]);
        // Clean up the title — remove common suffixes that aren't game names
        let cleaned = title
            .trim()
            .trim_end_matches(" - Google Chrome")
            .trim_end_matches(" - Mozilla Firefox")
            .trim_end_matches(" - Microsoft Edge")
            .trim_end_matches(" – Mozilla Firefox")
            .trim_end_matches(" - Visual Studio Code")
            .trim_end_matches(" - Discord")
            .to_string();
        // Sanitize for filesystem (remove characters invalid in filenames)
        let safe: String = cleaned.chars()
            .map(|c| match c {
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
                _ => c,
            })
            .collect();
        if safe.is_empty() {
            Ok("Desktop".to_string())
        } else {
            Ok(safe)
        }
    }
}

#[tauri::command]
pub async fn system_info() -> Result<serde_json::Value, String> {
    Ok(serde_json::json!({
        "platform": "win32",
        "arch": std::env::consts::ARCH,
        "totalMem": sysinfo_total_mem(),
        "freeMem": sysinfo_free_mem(),
        "cpus": num_cpus(),
    }))
}

// ── Hotkey suspend/resume ─────────────────────────────────────────────────────

#[tauri::command]
pub async fn hotkeys_suspend(app: AppHandle) -> Result<bool, String> {
    app.global_shortcut().unregister_all().map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
pub async fn hotkeys_resume(app: AppHandle, store: State<'_, SettingsStore>) -> Result<bool, String> {
    crate::register_hotkeys(&app, &store.get());
    Ok(true)
}

// ── Helper functions ──────────────────────────────────────────────────────────

fn ensure_output_folder(store: &SettingsStore) -> PathBuf {
    let settings = store.get();
    let folder = if settings.output_folder.is_empty() {
        let default = dirs::video_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
            .join("Clipsta");
        store.set_field(
            "outputFolder",
            serde_json::Value::String(default.to_string_lossy().to_string()),
        );
        default
    } else {
        PathBuf::from(&settings.output_folder)
    };
    let _ = std::fs::create_dir_all(&folder);
    folder
}

fn unique_path(folder: &std::path::Path, name: &str) -> PathBuf {
    let dest = folder.join(name);
    if !dest.exists() {
        return dest;
    }
    let stem = std::path::Path::new(name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let ext = std::path::Path::new(name)
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let mut i = 1;
    loop {
        let candidate = folder.join(format!("{} ({}){}", stem, i, if ext.is_empty() { String::new() } else { format!(".{}", ext) }));
        if !candidate.exists() {
            return candidate;
        }
        i += 1;
    }
}

fn resolve_target_res(setting: &str) -> Option<(u32, u32)> {
    match setting {
        "480p" => Some((854, 480)),
        "720p" => Some((1280, 720)),
        "1080p" => Some((1920, 1080)),
        "1440p" => Some((2560, 1440)),
        "4k" => Some((3840, 2160)),
        _ => None,
    }
}

fn resolve_game_bar_bitrate(resolution: &str, fps: u32) -> u32 {
    // Bitrates matched from actual NVIDIA ShadowPlay clip analysis.
    // Real ShadowPlay 720p60 clip measured at ~8 Mbps (8048 kb/s).
    let is60 = fps >= 50;
    match resolution {
        "480p" => if is60 { 4000 } else { 2500 },
        "720p" => if is60 { 8000 } else { 5000 },         // Measured from real ShadowPlay clip
        "1080p" => if is60 { 20000 } else { 12000 },
        "1440p" => if is60 { 50000 } else { 30000 },
        "4k" => if is60 { 80000 } else { 50000 },
        _ => if is60 { 8000 } else { 5000 },              // Default to 720p ShadowPlay style
    }
}

fn find_ffmpeg() -> String {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    if let Some(ref dir) = exe_dir {
        // Tauri bundles resources next to the exe
        let bundled = dir.join("ffmpeg.exe");
        if bundled.exists() {
            return bundled.to_string_lossy().to_string();
        }
        // Also check resources subfolder
        let resources = dir.join("resources").join("ffmpeg.exe");
        if resources.exists() {
            return resources.to_string_lossy().to_string();
        }
    }
    // Dev fallback
    let dev_path = PathBuf::from("C:\\Users\\scott\\clipsta-win-V1\\bin\\ffmpeg.exe");
    if dev_path.exists() {
        return dev_path.to_string_lossy().to_string();
    }
    "ffmpeg".to_string()
}

/// Public accessor for ffmpeg path, used by other modules.
pub fn find_ffmpeg_path() -> String {
    find_ffmpeg()
}

fn get_encoder_args(encoder: Option<&str>, fps: u32) -> (String, Vec<String>) {
    let keyint = fps * 2;
    match encoder.unwrap_or("auto") {
        "auto" | "NVENC (NVIDIA)" => (
            "h264_nvenc".into(),
            vec![
                "-preset".into(), "p1".into(),
                "-tune".into(), "ll".into(),
                "-rc".into(), "constqp".into(),
                "-qp".into(), "20".into(),
                "-pix_fmt".into(), "yuv420p".into(),
                "-g".into(), format!("{}", keyint),
                "-bf".into(), "0".into(),
                "-profile:v".into(), "high".into(),
            ],
        ),
        "AMF (AMD)" => (
            "h264_amf".into(),
            vec![
                "-quality".into(), "speed".into(),
                "-rc".into(), "cqp".into(),
                "-qp_i".into(), "22".into(),
                "-qp_p".into(), "22".into(),
                "-pix_fmt".into(), "yuv420p".into(),
                "-g".into(), format!("{}", keyint),
                "-bf".into(), "0".into(),
                "-profile:v".into(), "high".into(),
            ],
        ),
        "QuickSync (Intel)" => (
            "h264_qsv".into(),
            vec![
                "-preset".into(), "veryfast".into(),
                "-global_quality".into(), "22".into(),
                "-pix_fmt".into(), "yuv420p".into(),
                "-g".into(), format!("{}", keyint),
                "-bf".into(), "0".into(),
                "-profile:v".into(), "high".into(),
            ],
        ),
        _ => (
            "libx264".into(),
            vec![
                "-preset".into(), "ultrafast".into(),
                "-crf".into(), "23".into(),
                "-pix_fmt".into(), "yuv420p".into(),
                "-g".into(), format!("{}", keyint),
                "-bf".into(), "0".into(),
                "-profile:v".into(), "baseline".into(),
            ],
        ),
    }
}

/// Concatenate MP4 segments using FFmpeg concat demuxer.
/// ShadowPlay approach: segments are already encoded at target resolution
/// (e.g., 720p) by the capture pipeline. Clip saving is a fast stream-copy
/// concat — no transcoding needed. This makes saves instant like ShadowPlay.
async fn concat_segments(
    segment_paths: &[String],
    output_path: &str,
    ss_offset: f64,
    duration: f64,
    _target_resolution: Option<(u32, u32)>,
) -> Result<(), String> {
    let ffmpeg = find_ffmpeg();
    let temp_dir = std::env::temp_dir();
    let concat_list = temp_dir.join(format!("clipsta_concat_{}.txt", chrono::Local::now().format("%s%f")));

    let content: String = segment_paths
        .iter()
        .map(|p| format!("file '{}'", p.replace('\\', "/").replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&concat_list, &content).map_err(|e| e.to_string())?;

    let mut args: Vec<String> = Vec::new();
    // Input: concat demuxer
    args.extend([
        "-fflags".into(), "+genpts+discardcorrupt".into(),
        "-f".into(), "concat".into(),
        "-safe".into(), "0".into(),
        "-i".into(), concat_list.to_string_lossy().to_string(),
    ]);
    // -ss AFTER -i for frame-accurate seek
    if ss_offset > 0.0 {
        args.extend(["-ss".into(), format!("{:.2}", ss_offset)]);
    }
    if duration > 0.0 {
        args.extend(["-t".into(), format!("{:.1}", duration)]);
    }
    // Stream-copy: segments already have the correct resolution and codec.
    // This makes clip saves nearly instant (just copying bytes, no re-encoding).
    args.extend([
        "-c".into(), "copy".into(),
        "-fflags".into(), "+genpts".into(),
        "-avoid_negative_ts".into(), "make_zero".into(),
        "-movflags".into(), "+faststart".into(),
        "-y".into(), output_path.into(),
    ]);

    let output = {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            tokio::process::Command::new(&ffmpeg)
                .args(&args)
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .await
                .map_err(|e| format!("FFmpeg failed: {}", e))?
        }
        #[cfg(not(windows))]
        {
            tokio::process::Command::new(&ffmpeg)
                .args(&args)
                .output()
                .await
                .map_err(|e| format!("FFmpeg failed: {}", e))?
        }
    };

    let _ = std::fs::remove_file(&concat_list);

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("FFmpeg concat failed: {}", stderr.chars().rev().take(200).collect::<String>().chars().rev().collect::<String>()))
    }
}

fn sysinfo_total_mem() -> u64 {
    // Use kernel32 GlobalMemoryStatusEx via raw FFI
    #[repr(C)]
    #[allow(non_snake_case)]
    struct MEMORYSTATUSEX {
        dwLength: u32,
        dwMemoryLoad: u32,
        ullTotalPhys: u64,
        ullAvailPhys: u64,
        ullTotalPageFile: u64,
        ullAvailPageFile: u64,
        ullTotalVirtual: u64,
        ullAvailVirtual: u64,
        ullAvailExtendedVirtual: u64,
    }
    extern "system" {
        fn GlobalMemoryStatusEx(lpBuffer: *mut MEMORYSTATUSEX) -> i32;
    }
    unsafe {
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut status) != 0 {
            status.ullTotalPhys
        } else {
            0
        }
    }
}

fn sysinfo_free_mem() -> u64 {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct MEMORYSTATUSEX {
        dwLength: u32,
        dwMemoryLoad: u32,
        ullTotalPhys: u64,
        ullAvailPhys: u64,
        ullTotalPageFile: u64,
        ullAvailPageFile: u64,
        ullTotalVirtual: u64,
        ullAvailVirtual: u64,
        ullAvailExtendedVirtual: u64,
    }
    extern "system" {
        fn GlobalMemoryStatusEx(lpBuffer: *mut MEMORYSTATUSEX) -> i32;
    }
    unsafe {
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut status) != 0 {
            status.ullAvailPhys
        } else {
            0
        }
    }
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}


// ── MP4 Inspection commands ───────────────────────────────────────────────────

#[tauri::command]
pub async fn mp4_inspect(file_path: String) -> Result<crate::mp4_inspect::Mp4Info, String> {
    crate::mp4_inspect::inspect_mp4(&file_path)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn mp4_keyframes(file_path: String) -> Result<Vec<f64>, String> {
    let info = crate::mp4_inspect::inspect_mp4(&file_path)
        .await
        .map_err(|e| e.to_string())?;
    Ok(info.keyframes)
}

// ── Lossless Trim command ─────────────────────────────────────────────────────

#[tauri::command]
pub async fn lossless_trim_clip(
    input_path: String,
    output_path: String,
    start: f64,
    end: f64,
) -> Result<crate::lossless_trim::TrimResult, String> {
    // First get keyframes for the input file
    let info = crate::mp4_inspect::inspect_mp4(&input_path)
        .await
        .map_err(|e| format!("Failed to inspect MP4: {}", e))?;

    crate::lossless_trim::lossless_trim(&input_path, &output_path, start, end, &info.keyframes)
        .await
        .map_err(|e| e.to_string())
}
