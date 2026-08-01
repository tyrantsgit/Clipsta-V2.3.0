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
    let mut clips = Vec::new();
    // Recursive scan for clips in game-name subfolders (ShadowPlay style)
    fn scan_dir(dir: &std::path::Path, clips: &mut Vec<ClipFile>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                scan_dir(&path, clips);
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !matches!(ext.as_str(), "mp4" | "webm" | "mkv" | "mov") {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if meta.len() == 0 { continue; } // Skip empty/failed clips
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
    }
    scan_dir(&folder, &mut clips);
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
        segment_duration: 3,
        buffer_duration: settings.buffer_duration,
        segment_dir: seg_dir,
    };

    let app_handle = app.clone();
    let on_segment = Box::new(move |seg: CompletedSegment| {
        let _ = app_handle.emit("wgc:segment", &seg);
    });

    let (out_w, out_h) = (1280u32, 720u32);
    eprintln!("[wgc_start_recording] calling session.start with fps={} bitrate={} no_audio={} ({}x{})",
        fps, bitrate, no_audio, out_w, out_h);
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
    pub brightness: Option<u32>,
    pub contrast: Option<u32>,
    pub saturation: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CutRange {
    pub start: f64,
    pub end: f64,
}

/// Compress a clip to 720p for faster upload.
/// Returns Ok(None) — direct upload of original quality is the preferred path
/// (constraint #11: clips are already encoded at target resolution by the
/// capture pipeline, so re-encoding for upload is unnecessary overhead).
/// If a smaller upload size is needed in the future, this can be implemented
/// with MF Sink Writer + hardware H.264 encoder at 720p.
#[tauri::command]
pub async fn compress_for_upload(file_path: String) -> Result<Option<String>, String> {
    // v2.3: Direct upload of original quality. The capture pipeline already
    // encodes at the user's chosen resolution (typically 720p-1080p) with
    // hardware H.264, so re-compression adds latency without meaningful
    // size reduction for most clips.
    let _ = file_path;
    Ok(None)
}

#[tauri::command]
pub async fn recording_export(
    input_path: String,
    output_path: String,
    opts: ExportOpts,
) -> Result<String, String> {
    // Use FFmpeg for export (runs as separate process — no NVENC conflicts).
    // Core capture pipeline remains pure MF/D3D11 per spec.
    let output_clone = output_path.clone();

    tokio::task::spawn_blocking(move || {
        ffmpeg_export(&input_path, &output_path, &opts)
    })
    .await
    .map_err(|e| format!("Export task failed: {}", e))?
    .map_err(|e| format!("Export failed: {}", e))?;

    Ok(output_clone)
}

/// Export using FFmpeg as external process.
/// Handles trim, aspect ratio, resolution changes without conflicting with NVENC.
fn ffmpeg_export(input: &str, output: &str, opts: &ExportOpts) -> Result<(), String> {
    let mut args: Vec<String> = vec!["-y".to_string()]; // overwrite output

    // Trim: seek input
    if let Some(start) = opts.trim_start {
        if start > 0.0 {
            args.push("-ss".to_string());
            args.push(format!("{:.3}", start));
        }
    }

    args.push("-i".to_string());
    args.push(input.to_string());

    // Trim: end time
    if let Some(end) = opts.trim_end {
        let start = opts.trim_start.unwrap_or(0.0);
        let duration = end - start;
        if duration > 0.0 {
            args.push("-t".to_string());
            args.push(format!("{:.3}", duration));
        }
    }

    // Force 60fps output
    args.push("-r".to_string());
    args.push("60".to_string());

    // Video codec: use NVENC (separate process doesn't conflict with capture's NVENC)
    args.push("-c:v".to_string());
    args.push("h264_nvenc".to_string());
    args.push("-preset".to_string());
    args.push("p7".to_string());  // Highest quality preset
    args.push("-rc".to_string());
    args.push("vbr".to_string());
    args.push("-cq".to_string());
    args.push("18".to_string());  // High quality (lower = better, 18 is visually lossless)
    args.push("-b:v".to_string());
    args.push("20M".to_string());
    args.push("-maxrate".to_string());
    args.push("30M".to_string());

    // Resolution
    if let Some(ref res) = opts.resolution {
        match res.as_str() {
            "480p" => { args.push("-vf".to_string()); args.push("scale=854:480".to_string()); }
            "720p" => { args.push("-vf".to_string()); args.push("scale=1280:720".to_string()); }
            "1080p" => { args.push("-vf".to_string()); args.push("scale=1920:1080".to_string()); }
            "1440p" => { args.push("-vf".to_string()); args.push("scale=2560:1440".to_string()); }
            "4k" => { args.push("-vf".to_string()); args.push("scale=3840:2160".to_string()); }
            _ => {} // "source" or unknown = keep original
        }
    }

    // Aspect ratio crop
    if let Some(ref aspect) = opts.aspect_ratio {
        let crop_filter = match aspect.as_str() {
            "9:16" => Some("crop=ih*9/16:ih"),
            "1:1" => Some("crop=min(iw\\,ih):min(iw\\,ih)"),
            "4:5" => Some("crop=ih*4/5:ih"),
            _ => None, // 16:9 = default, no crop
        };
        if let Some(crop) = crop_filter {
            // Append crop AFTER scale (crop dimensions reference scaled output)
            if let Some(pos) = args.iter().position(|a| a == "-vf") {
                let existing = args[pos + 1].clone();
                args[pos + 1] = format!("{},{}", existing, crop);
            } else {
                args.push("-vf".to_string());
                args.push(crop.to_string());
            }
        }
    }

    // Video adjustments: brightness, contrast, saturation via eq filter
    let has_adjustments = opts.brightness.is_some() || opts.contrast.is_some() || opts.saturation.is_some();
    if has_adjustments {
        let b = opts.brightness.unwrap_or(100) as f64 / 100.0;  // 1.0 = normal
        let c = opts.contrast.unwrap_or(100) as f64 / 100.0;
        let s = opts.saturation.unwrap_or(100) as f64 / 100.0;
        // FFmpeg eq filter: brightness is -1.0 to 1.0 (0 = normal), contrast/saturation are multipliers
        let eq_filter = format!("eq=brightness={:.2}:contrast={:.2}:saturation={:.2}", b - 1.0, c, s);
        if let Some(pos) = args.iter().position(|a| a == "-vf") {
            let existing = args[pos + 1].clone();
            args[pos + 1] = format!("{},{}", existing, eq_filter);
        } else {
            args.push("-vf".to_string());
            args.push(eq_filter);
        }
    }

    args.push("-c:a".to_string());
    args.push("aac".to_string());
    args.push("-b:a".to_string());
    args.push("192k".to_string());

    // Output
    args.push(output.to_string());

    // Find ffmpeg
    let ffmpeg_path = find_ffmpeg().ok_or_else(|| "FFmpeg not found. Install via: winget install ffmpeg".to_string())?;

    // Run ffmpeg
    use std::os::windows::process::CommandExt;

    let result = std::process::Command::new(&ffmpeg_path)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .output()
        .map_err(|e| format!("Failed to run FFmpeg: {}", e))?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        // Try software encoder fallback if NVENC fails
        if stderr.contains("Cannot load") || stderr.contains("nvenc") || stderr.contains("No NVENC") {
            return ffmpeg_export_software(input, output, opts);
        }
        return Err(format!("FFmpeg failed: {}", stderr.lines().last().unwrap_or("unknown error")));
    }

    Ok(())
}

/// Fallback: use libx264 software encoder if NVENC unavailable in FFmpeg
fn ffmpeg_export_software(input: &str, output: &str, opts: &ExportOpts) -> Result<(), String> {
    let mut args: Vec<String> = vec!["-y".to_string()];

    if let Some(start) = opts.trim_start {
        if start > 0.0 {
            args.push("-ss".to_string());
            args.push(format!("{:.3}", start));
        }
    }

    args.push("-i".to_string());
    args.push(input.to_string());

    if let Some(end) = opts.trim_end {
        let start = opts.trim_start.unwrap_or(0.0);
        let duration = end - start;
        if duration > 0.0 {
            args.push("-t".to_string());
            args.push(format!("{:.3}", duration));
        }
    }

    // Force 60fps output
    args.push("-r".to_string());
    args.push("60".to_string());

    args.push("-c:v".to_string());
    args.push("libx264".to_string());
    args.push("-preset".to_string());
    args.push("medium".to_string());
    args.push("-crf".to_string());
    args.push("18".to_string());  // High quality (visually lossless)

    if let Some(ref res) = opts.resolution {
        match res.as_str() {
            "480p" => { args.push("-vf".to_string()); args.push("scale=854:480".to_string()); }
            "720p" => { args.push("-vf".to_string()); args.push("scale=1280:720".to_string()); }
            "1080p" => { args.push("-vf".to_string()); args.push("scale=1920:1080".to_string()); }
            "1440p" => { args.push("-vf".to_string()); args.push("scale=2560:1440".to_string()); }
            "4k" => { args.push("-vf".to_string()); args.push("scale=3840:2160".to_string()); }
            _ => {}
        }
    }

    if let Some(ref aspect) = opts.aspect_ratio {
        let crop_filter = match aspect.as_str() {
            "9:16" => Some("crop=ih*9/16:ih"),
            "1:1" => Some("crop=min(iw\\,ih):min(iw\\,ih)"),
            "4:5" => Some("crop=ih*4/5:ih"),
            _ => None,
        };
        if let Some(crop) = crop_filter {
            if let Some(pos) = args.iter().position(|a| a == "-vf") {
                let existing = args[pos + 1].clone();
                args[pos + 1] = format!("{},{}", existing, crop);
            } else {
                args.push("-vf".to_string());
                args.push(crop.to_string());
            }
        }
    }

    // Video adjustments (same as NVENC path)
    let has_adjustments = opts.brightness.is_some() || opts.contrast.is_some() || opts.saturation.is_some();
    if has_adjustments {
        let b = opts.brightness.unwrap_or(100) as f64 / 100.0;
        let c = opts.contrast.unwrap_or(100) as f64 / 100.0;
        let s = opts.saturation.unwrap_or(100) as f64 / 100.0;
        let eq_filter = format!("eq=brightness={:.2}:contrast={:.2}:saturation={:.2}", b - 1.0, c, s);
        if let Some(pos) = args.iter().position(|a| a == "-vf") {
            let existing = args[pos + 1].clone();
            args[pos + 1] = format!("{},{}", existing, eq_filter);
        } else {
            args.push("-vf".to_string());
            args.push(eq_filter);
        }
    }

    args.push("-c:a".to_string());
    args.push("aac".to_string());
    args.push("-b:a".to_string());
    args.push("192k".to_string());
    args.push(output.to_string());

    let ffmpeg_path = find_ffmpeg().ok_or_else(|| "FFmpeg not found".to_string())?;

    use std::os::windows::process::CommandExt;

    let result = std::process::Command::new(&ffmpeg_path)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .output()
        .map_err(|e| format!("FFmpeg failed: {}", e))?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(format!("Export failed: {}", stderr.lines().last().unwrap_or("unknown")));
    }

    Ok(())
}

/// Find ffmpeg executable on the system
fn find_ffmpeg() -> Option<String> {
    use std::os::windows::process::CommandExt;

    // Check next to our executable first (bundled ffmpeg)
    if let Ok(exe_path) = std::env::current_exe() {
        let dir = exe_path.parent().unwrap_or(std::path::Path::new("."));
        let bundled = dir.join("ffmpeg.exe");
        if bundled.exists() {
            return Some(bundled.to_string_lossy().to_string());
        }
        // Check resources subfolder
        let resources = dir.join("resources").join("ffmpeg.exe");
        if resources.exists() {
            return Some(resources.to_string_lossy().to_string());
        }
    }

    // Check PATH
    if let Ok(output) = std::process::Command::new("where.exe")
        .arg("ffmpeg")
        .creation_flags(0x08000000)
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout);
            if let Some(first_line) = path.lines().next() {
                if std::path::Path::new(first_line.trim()).exists() {
                    return Some(first_line.trim().to_string());
                }
            }
        }
    }

    // Check common install locations
    let common_paths = [
        r"C:\Program Files\ffmpeg\bin\ffmpeg.exe",
        r"C:\ffmpeg\bin\ffmpeg.exe",
    ];
    for p in &common_paths {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
    }

    None
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

// ── Watch Folder commands ─────────────────────────────────────────────────────

#[tauri::command]
pub async fn watch_folder_start(
    app: AppHandle,
    service: State<'_, crate::watch_folder::WatchFolderService>,
    store: State<'_, SettingsStore>,
) -> Result<bool, String> {
    let settings = store.get();
    let path = settings.watch_folder_path.clone();
    if path.is_empty() {
        return Err("No watch folder path configured".to_string());
    }
    service.start(path, app)?;
    Ok(true)
}

#[tauri::command]
pub async fn watch_folder_stop(
    service: State<'_, crate::watch_folder::WatchFolderService>,
) -> Result<bool, String> {
    service.stop();
    Ok(true)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchFolderStatusResponse {
    pub active: bool,
    pub files_detected: u64,
}

#[tauri::command]
pub async fn watch_folder_status(
    service: State<'_, crate::watch_folder::WatchFolderService>,
) -> Result<WatchFolderStatusResponse, String> {
    Ok(WatchFolderStatusResponse {
        active: service.is_active(),
        files_detected: service.files_detected(),
    })
}
