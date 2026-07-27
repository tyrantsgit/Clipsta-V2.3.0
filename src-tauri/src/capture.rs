//! Screen capture via FFmpeg gdigrab/ddagrab subprocess, recording to a
//! rolling set of short segments (not one continuous file).
//!
//! Segmentation matters for one reason: saving a clip must never touch the
//! file ffmpeg is actively writing. Earlier versions of this recorded to a
//! single continuous MKV and had to stop() the ffmpeg process to safely
//! seek from the end of it before extracting a clip — which meant every
//! "save last N seconds" hotkey press silently ended the background replay
//! buffer instead of leaving it running. Segments fix that: only fully
//! closed segment files are ever read from, so save operations are pure
//! reads that never interrupt the writer.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use parking_lot::Mutex;
use serde::Serialize;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize)]
pub struct CompletedSegment { pub path: String, pub index: u32, pub start_pts: f64, pub end_pts: f64, pub duration: f64 }

#[derive(Debug, Clone, Serialize)]
pub struct CaptureReadyInfo { pub width: u32, pub height: u32, pub fps: u32, pub segment_dir: String }

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub source_id: Option<String>, pub fps: u32, pub no_audio: bool,
    pub mic_device: Option<String>, pub loopback_device: Option<String>,
    pub target_width: Option<u32>, pub target_height: Option<u32>,
    pub bitrate_kbps: u32, pub segment_duration: u32, pub buffer_duration: u32, pub segment_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo { pub id: String, pub name: String, pub source_type: String, pub width: i32, pub height: i32 }

fn find_ffmpeg() -> String {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    if let Some(ref dir) = exe_dir {
        let bundled = dir.join("ffmpeg.exe");
        if bundled.exists() { return bundled.to_string_lossy().to_string(); }
        let resources = dir.join("resources").join("ffmpeg.exe");
        if resources.exists() { return resources.to_string_lossy().to_string(); }
    }
    "ffmpeg".to_string()
}

fn get_screen_size() -> (u32, u32) {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        let w = GetSystemMetrics(SM_CXSCREEN) as u32;
        let h = GetSystemMetrics(SM_CYSCREEN) as u32;
        if w > 0 && h > 0 { (w, h) } else { (1920, 1080) }
    }
}

// ── CaptureSession ────────────────────────────────────────────────────────────

pub struct CaptureSession {
    pub is_recording: Arc<AtomicBool>,
    pub is_saving: Arc<AtomicBool>,
    pub dropped_frames: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
    segments: Arc<Mutex<Vec<CompletedSegment>>>,
    segment_dir: Arc<Mutex<Option<PathBuf>>>,
    segment_duration: Arc<Mutex<u32>>,
    recording_start: Arc<Mutex<Option<Instant>>>,
    ffmpeg_child: Arc<Mutex<Option<Child>>>,
    /// How many seconds AFTER video did audio actually start recording.
    /// Used to correct A/V sync at clip-save time.
    audio_start_delay_secs: Arc<Mutex<f64>>,
}

impl Default for CaptureSession {
    fn default() -> Self {
        Self {
            is_recording: Arc::new(AtomicBool::new(false)),
            is_saving: Arc::new(AtomicBool::new(false)),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            segments: Arc::new(Mutex::new(Vec::new())),
            segment_dir: Arc::new(Mutex::new(None)),
            segment_duration: Arc::new(Mutex::new(5)),
            recording_start: Arc::new(Mutex::new(None)),
            ffmpeg_child: Arc::new(Mutex::new(None)),
            audio_start_delay_secs: Arc::new(Mutex::new(0.0)),
        }
    }
}

impl CaptureSession {
    pub fn new() -> Self { Self::default() }

    pub fn start(&self, opts: CaptureOptions, on_segment: Box<dyn Fn(CompletedSegment) + Send + 'static>) -> Result<CaptureReadyInfo> {
        if self.is_recording.load(Ordering::Relaxed) { anyhow::bail!("Already recording"); }
        self.stop_flag.store(false, Ordering::SeqCst);

        let (_native_w, _native_h) = get_screen_size();
        let fps = opts.fps;
        let seg_dir = opts.segment_dir.clone();
        std::fs::create_dir_all(&seg_dir)?;

        let segment_duration = opts.segment_duration.max(1);
        *self.segment_duration.lock() = segment_duration;

        let seg_pattern = seg_dir.join("seg_%05d.mp4").to_string_lossy().to_string().replace('\\', "/");
        let ffmpeg = find_ffmpeg();

        let fps_str = fps.to_string();
        let seg_dur_str = segment_duration.to_string();
        let gop_str = (fps * 2).to_string();

        // Target resolution — ddagrab captures directly at this size (GPU-side scaling)
        let target_w = opts.target_width.unwrap_or(1280);
        let target_h = opts.target_height.unwrap_or(720);

        let ddagrab_input = format!("ddagrab=framerate={}:draw_mouse=1:video_size={}x{}", fps, target_w, target_h);

        // ── BUILD FFMPEG ARGS ──
        // ShadowPlay-style recording: ddagrab captures directly at target
        // resolution (e.g., 1280x720) using GPU-side scaling, then NVENC
        // encodes with minimal overhead. Clips in the buffer are already
        // at final resolution — saving is instant (stream-copy concat).
        // Audio captured by WASAPI written to a named pipe that FFmpeg reads.
        // Named pipe has a large buffer (4MB) so it never blocks/deadlocks.
        // Both audio and video go through FFmpeg = single clock = perfect sync.
        let has_audio = !opts.no_audio;

        // Create a Windows named pipe for audio BEFORE starting FFmpeg
        let pipe_name = format!("\\\\.\\pipe\\clipsta_audio_{}", std::process::id());
        let pipe_path_for_ffmpeg = pipe_name.clone();

        let mut args: Vec<String> = Vec::new();

        // Video input
        args.extend([
            "-y".to_string(),
            "-f".to_string(), "lavfi".to_string(),
            "-i".to_string(), ddagrab_input,
        ]);

        // Audio input from named pipe (FFmpeg reads it as raw PCM)
        if has_audio {
            args.extend([
                "-f".to_string(), "s16le".to_string(),
                "-ar".to_string(), "48000".to_string(),
                "-ac".to_string(), "2".to_string(),
                "-i".to_string(), pipe_path_for_ffmpeg.clone(),
            ]);
            args.extend([
                "-map".to_string(), "0:v:0".to_string(),
                "-map".to_string(), "1:a:0".to_string(),
            ]);
        }

        // NOTE: No -vf scale filter here. ddagrab captures directly at the
        // target resolution via video_size parameter (GPU-side scaling).
        // This is identical to how ShadowPlay works — no CPU involvement.

        // Video encoder — Settings matched from actual ShadowPlay output:
        // Analyzed: "Battlefield 6 2026.07.26 - 19.56.14.04.DVR.mp4"
        // - H.264 High profile, yuv420p, bt709, progressive
        // - ~8 Mbps for 720p60 (CBR)
        // - 60 fps, no B-frames, low-latency
        // - GOP = 2x fps (keyframe every 2 seconds)
        // Using CBR to match ShadowPlay's consistent bitrate behavior.
        let bitrate_str = format!("{}k", opts.bitrate_kbps);
        let maxrate_str = format!("{}k", opts.bitrate_kbps + 1000);
        let bufsize_str = format!("{}k", opts.bitrate_kbps * 2);
        args.extend([
            "-c:v".to_string(), "h264_nvenc".to_string(),
            "-preset".to_string(), "p1".to_string(),
            "-tune".to_string(), "ll".to_string(),
            "-rc".to_string(), "cbr".to_string(),
            "-b:v".to_string(), bitrate_str,
            "-maxrate".to_string(), maxrate_str,
            "-bufsize".to_string(), bufsize_str,
            "-r".to_string(), fps_str.clone(),
            "-video_track_timescale".to_string(), "90000".to_string(), // Match ShadowPlay's 90k tbn
            "-g".to_string(), gop_str,
            "-bf".to_string(), "0".to_string(),
            "-profile:v".to_string(), "high".to_string(),
        ]);

        // Audio encoder — AAC 192kbps (ShadowPlay uses similar quality)
        if has_audio {
            args.extend([
                "-c:a".to_string(), "aac".to_string(),
                "-b:a".to_string(), "192k".to_string(),
            ]);
        }

        // Output segments
        args.extend([
            "-f".to_string(), "segment".to_string(),
            "-segment_time".to_string(), seg_dur_str,
            "-segment_format".to_string(), "mp4".to_string(),
            "-reset_timestamps".to_string(), "1".to_string(),
            "-strftime".to_string(), "0".to_string(),
            seg_pattern,
        ]);

        eprintln!("[capture] FFmpeg: {} {}", ffmpeg, args.join(" "));

        // ── AUDIO: Create named pipe and start WASAPI writing to it ──
        // The named pipe has a 4MB buffer. FFmpeg reads from it as an input.
        // Both audio and video go through FFmpeg's internal clock = perfect sync.
        // Unlike stdin pipe:0, named pipes don't deadlock because FFmpeg opens
        // them as a regular file input with its own read thread.
        if has_audio {
            let stop_audio = self.stop_flag.clone();
            let mic_dev = opts.mic_device.clone();
            let lb_dev = opts.loopback_device.clone();
            let pipe_name_audio = pipe_name.clone();

            // Start WASAPI writer FIRST (creates the named pipe server)
            // FFmpeg will connect to it when it starts.
            thread::spawn(move || {
                use crate::audio::WasapiCapture;

                // Use raw Win32 API for named pipes (avoids windows crate feature issues)
                #[link(name = "kernel32")]
                extern "system" {
                    fn CreateNamedPipeA(name: *const u8, open_mode: u32, pipe_mode: u32, max_instances: u32, out_buf: u32, in_buf: u32, timeout: u32, security: *const u8) -> isize;
                    fn ConnectNamedPipe(pipe: isize, overlapped: *const u8) -> i32;
                    fn WriteFile(handle: isize, buf: *const u8, len: u32, written: *mut u32, overlapped: *const u8) -> i32;
                    fn CloseHandle(handle: isize) -> i32;
                    fn GetLastError() -> u32;
                }

                const PIPE_ACCESS_OUTBOUND: u32 = 0x00000002;
                const PIPE_TYPE_BYTE: u32 = 0x00000000;
                const PIPE_WAIT: u32 = 0x00000000;
                const INVALID_HANDLE: isize = -1;

                let pipe_cstr = format!("{}\0", pipe_name_audio);
                let pipe_handle = unsafe {
                    CreateNamedPipeA(
                        pipe_cstr.as_ptr(),
                        PIPE_ACCESS_OUTBOUND,
                        PIPE_TYPE_BYTE | PIPE_WAIT,
                        1,                    // max instances
                        4 * 1024 * 1024,      // 4MB output buffer
                        0,                    // input buffer
                        0,                    // default timeout
                        std::ptr::null(),     // default security
                    )
                };

                if pipe_handle == INVALID_HANDLE {
                    eprintln!("[audio] Failed to create named pipe, error={}", unsafe { GetLastError() });
                    return;
                }
                eprintln!("[audio] Named pipe created: {}", pipe_name_audio);

                // Wait for FFmpeg to connect (blocking)
                let result = unsafe { ConnectNamedPipe(pipe_handle, std::ptr::null()) };
                if result == 0 {
                    let err = unsafe { GetLastError() };
                    if err != 535 { // ERROR_PIPE_CONNECTED (already connected)
                        eprintln!("[audio] ConnectNamedPipe failed, error={}", err);
                        unsafe { CloseHandle(pipe_handle); }
                        return;
                    }
                }
                eprintln!("[audio] FFmpeg connected to audio pipe");

                // Write WASAPI audio directly to the named pipe
                let pipe_handle_shared = Arc::new(Mutex::new(pipe_handle));
                let pipe_w = pipe_handle_shared.clone();
                let stop_w = stop_audio.clone();

                let _ = WasapiCapture::capture_to_callback(stop_audio, mic_dev, lb_dev, move |chunk: &[f32]| {
                    if stop_w.load(Ordering::Relaxed) { return; }
                    let i16_data: Vec<u8> = chunk.iter()
                        .flat_map(|&s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
                        .collect();
                    // Blocking lock is safe here: the 4MB named pipe buffer means WriteFile
                    // returns immediately unless the buffer is completely full (which would
                    // require FFmpeg to not read for 10+ seconds — impossible since it's
                    // encoding 60fps video continuously). Unlike stdin pipe:0, the named pipe
                    // has an independent read thread in FFmpeg that drains it constantly.
                    let handle = pipe_w.lock();
                    let mut written: u32 = 0;
                    unsafe {
                        WriteFile(*handle, i16_data.as_ptr(), i16_data.len() as u32, &mut written, std::ptr::null());
                    }
                });

                // Cleanup
                unsafe { CloseHandle(pipe_handle); }
            });

            // Give the pipe server a moment to start before FFmpeg tries to connect
            thread::sleep(Duration::from_millis(100));
        }

        // Spawn FFmpeg (it will connect to the named pipe for audio input)
        #[cfg(windows)]
        let child = {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            Command::new(&ffmpeg)
                .args(&args)
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| anyhow::anyhow!("FFmpeg failed: {}", e))?
        };

        #[cfg(not(windows))]
        let child = Command::new(&ffmpeg).args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| anyhow::anyhow!("FFmpeg failed: {}", e))?;

        *self.ffmpeg_child.lock() = Some(child);
        *self.recording_start.lock() = Some(Instant::now());
        *self.segment_dir.lock() = Some(seg_dir.clone());
        *self.segments.lock() = Vec::new();
        *self.audio_start_delay_secs.lock() = 0.0;
        self.is_recording.store(true, Ordering::SeqCst);

        // ── SEGMENT WATCHER ──
        // Polls the segment directory to detect newly-closed segments.
        {
            let seg_dir_watch = seg_dir.clone();
            let stop_watch = self.stop_flag.clone();
            let segments_watch = self.segments.clone();
            let buffer_duration = opts.buffer_duration.max(segment_duration);
            let seg_dur_f = segment_duration as f64;

            thread::spawn(move || {
                let mut known_indices: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
                loop {
                    if stop_watch.load(Ordering::Relaxed) { break; }
                    thread::sleep(Duration::from_millis(400));

                    let mut found: Vec<(u32, PathBuf)> = match std::fs::read_dir(&seg_dir_watch) {
                        Ok(rd) => rd.filter_map(|e| e.ok())
                            .filter_map(|e| {
                                let path = e.path();
                                let name = path.file_stem()?.to_str()?.to_string();
                                let idx: u32 = name.strip_prefix("seg_")?.parse().ok()?;
                                Some((idx, path))
                            })
                            .collect(),
                        Err(_) => continue,
                    };
                    if found.is_empty() { continue; }
                    found.sort_by_key(|(idx, _)| *idx);

                    // The highest index is still being written by ffmpeg — never
                    // treat it as a closed, readable segment.
                    let writing_idx = found.last().map(|(idx, _)| *idx);

                    for (idx, path) in &found {
                        if Some(*idx) == writing_idx { continue; }
                        if known_indices.contains(idx) { continue; }
                        known_indices.insert(*idx);

                        let start_pts = *idx as f64 * seg_dur_f;
                        let end_pts = start_pts + seg_dur_f;
                        let seg = CompletedSegment {
                            path: path.to_string_lossy().to_string().replace('\\', "/"),
                            index: *idx,
                            start_pts,
                            end_pts,
                            duration: seg_dur_f,
                        };
                        segments_watch.lock().push(seg.clone());
                        on_segment(seg);
                    }

                    // Prune segments older than the configured replay buffer
                    // window so disk usage stays bounded, matching the
                    // buffer_duration setting instead of ignoring it.
                    let newest_end = found.last().map(|(idx, _)| (*idx as f64 + 1.0) * seg_dur_f).unwrap_or(0.0);
                    let cutoff = newest_end - buffer_duration as f64;
                    let mut segs = segments_watch.lock();
                    segs.retain(|s| {
                        if s.end_pts < cutoff {
                            let _ = std::fs::remove_file(&s.path);
                            false
                        } else {
                            true
                        }
                    });
                }
            });
        }

        Ok(CaptureReadyInfo { width: target_w, height: target_h, fps, segment_dir: seg_dir.to_string_lossy().to_string() })
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.is_recording.store(false, Ordering::SeqCst);
        // Send 'q' to FFmpeg to stop gracefully
        if let Some(mut child) = self.ffmpeg_child.lock().take() {
            if let Some(ref mut stdin) = child.stdin {
                use std::io::Write;
                let _ = stdin.write_all(b"q");
            }
            // Give it time to flush, then force kill
            let _handle = thread::spawn(move || {
                thread::sleep(Duration::from_secs(3));
                let _ = child.kill();
            });
        }
    }

    /// Real elapsed recording time, used for audio seek math at save time.
    pub fn elapsed_secs(&self) -> Option<f64> {
        self.recording_start.lock().map(|start| start.elapsed().as_secs_f64())
    }

    pub fn get_audio_file(&self) -> Option<String> {
        let dir = self.segment_dir.lock().clone()?;
        let path = dir.join("audio.raw").to_string_lossy().to_string().replace('\\', "/");
        if std::fs::metadata(&path).map(|m| m.len() > 1000).unwrap_or(false) {
            Some(path)
        } else {
            None
        }
    }

    /// Real closed segments only — never includes the segment ffmpeg is
    /// currently writing. Safe to read from at any time without stopping
    /// or otherwise touching the active recording.
    pub fn get_segments(&self) -> Vec<CompletedSegment> {
        let mut segs: Vec<CompletedSegment> = self.segments.lock()
            .iter()
            .filter(|s| std::path::Path::new(&s.path).exists())
            .cloned()
            .collect();
        segs.sort_by_key(|s| s.index);
        segs
    }

    /// Picks up the final segment file that was still being written when
    /// `stop()` was called. The watcher thread deliberately never treats the
    /// newest-indexed file as "closed" while recording is live (it might
    /// still be growing) — but once ffmpeg has actually exited, that file is
    /// finished and safe to include. Call this only after confirming the
    /// ffmpeg process has exited (e.g. after `stop()` plus a short delay).
    pub fn finalize_pending_segments(&self) {
        let Some(dir) = self.segment_dir.lock().clone() else { return };
        let seg_dur = *self.segment_duration.lock() as f64;
        let mut known: std::collections::BTreeSet<u32> =
            self.segments.lock().iter().map(|s| s.index).collect();

        let mut found: Vec<(u32, PathBuf)> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let path = e.path();
                    let name = path.file_stem()?.to_str()?.to_string();
                    let idx: u32 = name.strip_prefix("seg_")?.parse().ok()?;
                    Some((idx, path))
                })
                .collect(),
            Err(_) => return,
        };
        found.sort_by_key(|(idx, _)| *idx);

        for (idx, path) in found {
            if known.contains(&idx) { continue; }
            // A finished-but-empty/truncated segment (e.g. killed mid-write)
            // isn't worth including.
            if std::fs::metadata(&path).map(|m| m.len() < 1024).unwrap_or(true) { continue; }
            known.insert(idx);
            let start_pts = idx as f64 * seg_dur;
            self.segments.lock().push(CompletedSegment {
                path: path.to_string_lossy().to_string().replace('\\', "/"),
                index: idx,
                start_pts,
                end_pts: start_pts + seg_dur,
                duration: seg_dur,
            });
        }
    }

    pub fn get_segment_dir(&self) -> Option<PathBuf> { self.segment_dir.lock().clone() }

    /// Get the measured delay between video start and first audio sample.
    pub fn audio_start_delay(&self) -> f64 { *self.audio_start_delay_secs.lock() }

    pub fn cleanup(&self) {
        if let Some(d) = self.segment_dir.lock().take() { let _ = std::fs::remove_dir_all(&d); }
        *self.segments.lock() = Vec::new();
        *self.recording_start.lock() = None;
    }
}

// ── Source Listing ────────────────────────────────────────────────────────────

pub fn list_sources() -> Vec<SourceInfo> {
    let mut sources = Vec::new();
    unsafe {
        use windows::Win32::Foundation::{HWND, LPARAM, RECT};
        use windows::Win32::Graphics::Gdi::*;
        use windows_core::BOOL;
        extern "system" fn mon_cb(hmon: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
            let list = unsafe { &mut *(lp.0 as *mut Vec<SourceInfo>) };
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if unsafe { GetMonitorInfoW(hmon, &mut info.monitorInfo).as_bool() } {
                let name = String::from_utf16_lossy(&info.szDevice.iter().take_while(|&&c| c != 0).cloned().collect::<Vec<_>>());
                let r = &info.monitorInfo.rcMonitor;
                list.push(SourceInfo { id: format!("monitor:{}", hmon.0 as usize), name: format!("Display {}", name.trim()), source_type: "monitor".into(), width: r.right - r.left, height: r.bottom - r.top });
            }
            BOOL(1)
        }
        let _ = EnumDisplayMonitors(None, None, Some(mon_cb), LPARAM(&mut sources as *mut _ as isize));
        use windows::Win32::UI::WindowsAndMessaging::*;
        extern "system" fn win_cb(hwnd: HWND, lp: LPARAM) -> BOOL {
            let list = unsafe { &mut *(lp.0 as *mut Vec<SourceInfo>) };
            if !unsafe { IsWindowVisible(hwnd).as_bool() } { return BOOL(1); }
            let mut t = [0u16; 512]; let len = unsafe { GetWindowTextW(hwnd, &mut t) };
            if len == 0 { return BOOL(1); }
            let title = String::from_utf16_lossy(&t[..len as usize]);
            let mut r = RECT::default();
            let _ = unsafe { GetWindowRect(hwnd, &mut r) };
            if (r.right - r.left) < 150 || (r.bottom - r.top) < 150 { return BOOL(1); }
            list.push(SourceInfo { id: format!("hwnd:{}", hwnd.0 as usize), name: title, source_type: "window".into(), width: r.right - r.left, height: r.bottom - r.top });
            BOOL(1)
        }
        let _ = EnumWindows(Some(win_cb), LPARAM(&mut sources as *mut _ as isize));
    }
    sources
}
