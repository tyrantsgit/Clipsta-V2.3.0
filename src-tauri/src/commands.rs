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
    // v2.3: Media Foundation export with trim + re-encode at target resolution.
    // Complex filter chains (cuts, crops) are deferred to a future version.
    // For now, supports: trim (start/end) + resolution scaling + re-encode via MF.
    let output_clone = output_path.clone();

    tokio::task::spawn_blocking(move || {
        mf_export_trim(&input_path, &output_path, &opts)
    })
    .await
    .map_err(|e| format!("Export task failed: {}", e))?
    .map_err(|e| format!("Export failed: {}", e))?;

    Ok(output_clone)
}

/// Media Foundation-based export: trim + re-encode at target resolution.
fn mf_export_trim(input: &str, output: &str, opts: &ExportOpts) -> Result<(), String> {
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::*;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
            .map_err(|e| format!("MFStartup failed: {}", e))?;

        let result = mf_export_inner(input, output, opts);

        let _ = MFShutdown();
        result
    }
}

unsafe fn mf_export_inner(input: &str, output: &str, opts: &ExportOpts) -> Result<(), String> {
    use windows::core::{GUID, PCWSTR};
    use windows::Win32::Media::MediaFoundation::*;

    let wide_input: Vec<u16> = input.encode_utf16().chain(std::iter::once(0)).collect();
    let wide_output: Vec<u16> = output.encode_utf16().chain(std::iter::once(0)).collect();

    // Create Source Reader with decoding enabled (we need raw frames for re-encode)
    let mut reader_attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut reader_attrs, 1)
        .map_err(|e| format!("MFCreateAttributes failed: {}", e))?;
    let reader_attrs = reader_attrs.unwrap();
    reader_attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1).ok();

    let reader: IMFSourceReader =
        MFCreateSourceReaderFromURL(PCWSTR(wide_input.as_ptr()), &reader_attrs)
            .map_err(|e| format!("MFCreateSourceReaderFromURL failed: {}", e))?;

    // Configure reader to output uncompressed video (NV12 for hardware encode)
    let decode_type: IMFMediaType = MFCreateMediaType()
        .map_err(|e| format!("MFCreateMediaType failed: {}", e))?;
    decode_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
    decode_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| e.to_string())?;
    reader
        .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &decode_type)
        .map_err(|e| format!("SetCurrentMediaType (decode) failed: {}", e))?;

    // Get the actual decoded format to determine input dimensions
    let actual_type: IMFMediaType = reader
        .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
        .map_err(|e| format!("GetCurrentMediaType failed: {}", e))?;

    let frame_size = actual_type.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
    let src_width = (frame_size >> 32) as u32;
    let src_height = (frame_size & 0xFFFFFFFF) as u32;

    let frame_rate = actual_type.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(60 << 32 | 1);
    let fps_num = (frame_rate >> 32) as u32;
    let fps_den = (frame_rate & 0xFFFFFFFF) as u32;

    // Determine output resolution
    let (out_width, out_height) = if let Some(ref res) = opts.resolution {
        resolve_target_res(res).unwrap_or((src_width, src_height))
    } else {
        (src_width, src_height)
    };

    // Use requested FPS or source FPS
    let out_fps = opts.fps.unwrap_or(if fps_den > 0 { fps_num / fps_den } else { 60 });

    // Create Sink Writer
    let mut writer_attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut writer_attrs, 1)
        .map_err(|e| format!("MFCreateAttributes writer failed: {}", e))?;
    let writer_attrs = writer_attrs.unwrap();
    writer_attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1).ok();

    let writer: IMFSinkWriter =
        MFCreateSinkWriterFromURL(PCWSTR(wide_output.as_ptr()), None, &writer_attrs)
            .map_err(|e| format!("MFCreateSinkWriterFromURL failed: {}", e))?;

    // Output video type: H.264
    let out_video_type: IMFMediaType = MFCreateMediaType()
        .map_err(|e| format!("MFCreateMediaType out_video failed: {}", e))?;
    out_video_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
    out_video_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(|e| e.to_string())?;
    out_video_type.SetUINT32(&MF_MT_AVG_BITRATE, 8_000_000).map_err(|e| e.to_string())?;
    out_video_type.SetUINT64(&MF_MT_FRAME_SIZE, ((out_width as u64) << 32) | out_height as u64).map_err(|e| e.to_string())?;
    out_video_type.SetUINT64(&MF_MT_FRAME_RATE, ((out_fps as u64) << 32) | 1u64).map_err(|e| e.to_string())?;
    out_video_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| e.to_string())?;
    out_video_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32).map_err(|e| e.to_string())?;

    let video_stream_idx = writer.AddStream(&out_video_type)
        .map_err(|e| format!("AddStream video failed: {}", e))?;

    // Input type for the writer (uncompressed NV12 at output dimensions)
    let in_video_type: IMFMediaType = MFCreateMediaType()
        .map_err(|e| format!("MFCreateMediaType in_video failed: {}", e))?;
    in_video_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
    in_video_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| e.to_string())?;
    in_video_type.SetUINT64(&MF_MT_FRAME_SIZE, ((out_width as u64) << 32) | out_height as u64).map_err(|e| e.to_string())?;
    in_video_type.SetUINT64(&MF_MT_FRAME_RATE, ((out_fps as u64) << 32) | 1u64).map_err(|e| e.to_string())?;
    in_video_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| e.to_string())?;

    writer.SetInputMediaType(video_stream_idx, &in_video_type, None)
        .map_err(|e| format!("SetInputMediaType video failed: {}", e))?;

    // Configure audio passthrough if present
    let mut audio_stream_idx: u32 = 0;
    let has_audio = if let Ok(_audio_type) = reader
        .GetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32)
    {
        // Output audio as AAC
        let out_audio_type: IMFMediaType = MFCreateMediaType()
            .map_err(|e| format!("MFCreateMediaType audio failed: {}", e))?;
        out_audio_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(|e| e.to_string())?;
        out_audio_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).map_err(|e| e.to_string())?;
        out_audio_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(|e| e.to_string())?;
        out_audio_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000).map_err(|e| e.to_string())?;
        out_audio_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).map_err(|e| e.to_string())?;
        out_audio_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24000).map_err(|e| e.to_string())?;

        if let Ok(idx) = writer.AddStream(&out_audio_type) {
            audio_stream_idx = idx;
            // Set PCM as input to the audio encoder
            let in_audio_type: IMFMediaType = MFCreateMediaType()
                .map_err(|e| format!("MFCreateMediaType in_audio failed: {}", e))?;
            in_audio_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(|e| e.to_string())?;
            in_audio_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).map_err(|e| e.to_string())?;
            in_audio_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(|e| e.to_string())?;
            in_audio_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000).map_err(|e| e.to_string())?;
            in_audio_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).map_err(|e| e.to_string())?;

            // Tell the source reader to decode audio to PCM
            let _ = reader.SetCurrentMediaType(
                MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
                None,
                &in_audio_type,
            );

            let _ = writer.SetInputMediaType(audio_stream_idx, &in_audio_type, None);
            true
        } else {
            false
        }
    } else {
        false
    };

    // Seek to trim start
    let trim_start = opts.trim_start.unwrap_or(0.0);
    let trim_end = opts.trim_end;

    if trim_start > 0.0 {
        let start_100ns = (trim_start * 10_000_000.0) as i64;
        let start_pv = make_propvariant_i64_export(start_100ns);
        reader.SetCurrentPosition(&GUID::zeroed(), &start_pv)
            .map_err(|e| format!("SetCurrentPosition failed: {}", e))?;
    }

    // Begin writing
    writer.BeginWriting()
        .map_err(|e| format!("BeginWriting failed: {}", e))?;

    let start_100ns = (trim_start * 10_000_000.0) as i64;
    let end_100ns = trim_end.map(|e| (e * 10_000_000.0) as i64);

    // Read samples and write them
    loop {
        let mut stream_index: u32 = 0;
        let mut flags: u32 = 0;
        let mut timestamp: i64 = 0;
        let mut sample: Option<IMFSample> = None;

        let hr = reader.ReadSample(
            MF_SOURCE_READER_ANY_STREAM.0 as u32,
            0,
            Some(&mut stream_index),
            Some(&mut flags),
            Some(&mut timestamp),
            Some(&mut sample),
        );

        if hr.is_err() {
            break;
        }

        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }

        // Check if we've passed the trim end
        if let Some(end) = end_100ns {
            if timestamp > end {
                break;
            }
        }

        if let Some(ref s) = sample {
            // Adjust timestamp relative to trim start
            let adjusted = timestamp - start_100ns;
            if adjusted < 0 {
                continue;
            }
            let _ = s.SetSampleTime(adjusted);

            // Route to correct stream
            let is_video = stream_index == 0
                || stream_index == MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

            if is_video {
                let _ = writer.WriteSample(video_stream_idx, s);
            } else if has_audio {
                let _ = writer.WriteSample(audio_stream_idx, s);
            }
        }
    }

    writer.Finalize()
        .map_err(|e| format!("Finalize failed: {}", e))?;

    Ok(())
}

/// Create a PROPVARIANT containing an i64 value (VT_I8) for seeking in export.
unsafe fn make_propvariant_i64_export(value: i64) -> windows::Win32::System::Com::StructuredStorage::PROPVARIANT {
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Variant::VT_I8;
    let mut pv: PROPVARIANT = std::mem::zeroed();
    (*pv.Anonymous.Anonymous).vt = VT_I8;
    (*pv.Anonymous.Anonymous).Anonymous.hVal = value;
    pv
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
