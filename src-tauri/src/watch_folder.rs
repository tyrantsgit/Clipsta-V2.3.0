//! Watch Folder Service — monitors a directory for new video files and emits events.
//!
//! Uses simple polling (every 5 seconds) for reliability on Windows.
//! Files must stabilize (stop growing for 3+ seconds) before being considered ready.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// Video file extensions we monitor for.
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "mkv"];

/// How often to poll the directory (seconds).
const POLL_INTERVAL_SECS: u64 = 5;

/// How long a file must remain the same size before it's considered complete (seconds).
const STABILIZE_SECS: u64 = 3;

/// Event payload emitted when a new file is detected and stable.
#[derive(Debug, Clone, Serialize)]
pub struct WatchFolderNewFile {
    pub path: String,
    pub name: String,
    pub size: u64,
}

/// Internal state shared between the polling task and the control API.
#[derive(Debug)]
struct WatchFolderInner {
    /// Whether the service is currently active.
    active: bool,
    /// The directory being monitored.
    watch_path: PathBuf,
    /// Files that have already been processed (won't be re-emitted).
    seen_files: HashSet<PathBuf>,
    /// Files currently being checked for stability: path -> (last_known_size, first_seen_at_that_size).
    pending_files: std::collections::HashMap<PathBuf, (u64, Instant)>,
    /// Total number of files detected and emitted since service started.
    files_detected: u64,
    /// Timestamp when the service was started (used to filter old files).
    started_at: SystemTime,
}

/// Thread-safe watch folder service managed as Tauri state.
#[derive(Clone)]
pub struct WatchFolderService {
    inner: Arc<Mutex<WatchFolderInner>>,
    /// Handle to signal the background task to stop.
    stop_signal: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl WatchFolderService {
    /// Create a new (inactive) service instance.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(WatchFolderInner {
                active: false,
                watch_path: PathBuf::new(),
                seen_files: HashSet::new(),
                pending_files: std::collections::HashMap::new(),
                files_detected: 0,
                started_at: SystemTime::now(),
            })),
            stop_signal: Arc::new(Mutex::new(None)),
        }
    }

    /// Start monitoring the given directory. Returns an error if already active or path is invalid.
    pub fn start(&self, path: String, app: AppHandle) -> Result<(), String> {
        let watch_path = PathBuf::from(&path);
        if !watch_path.is_dir() {
            return Err(format!("Watch folder path does not exist or is not a directory: {}", path));
        }

        {
            let mut inner = self.inner.lock();
            if inner.active {
                return Err("Watch folder service is already running".to_string());
            }
            inner.active = true;
            inner.watch_path = watch_path.clone();
            inner.seen_files.clear();
            inner.pending_files.clear();
            inner.files_detected = 0;
            inner.started_at = SystemTime::now();
        }

        // Seed seen_files with existing files so we only detect NEW ones
        if let Ok(entries) = std::fs::read_dir(&watch_path) {
            let mut inner = self.inner.lock();
            for entry in entries.flatten() {
                let path = entry.path();
                if is_video_file(&path) {
                    inner.seen_files.insert(path);
                }
            }
        }

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        {
            let mut sig = self.stop_signal.lock();
            *sig = Some(stop_tx);
        }

        let inner = self.inner.clone();
        tokio::spawn(poll_loop(inner, app, stop_rx));

        eprintln!("[watch_folder] started monitoring: {}", path);
        Ok(())
    }

    /// Stop the monitoring task.
    pub fn stop(&self) {
        {
            let mut inner = self.inner.lock();
            inner.active = false;
        }
        // Send stop signal to the background task
        let mut sig = self.stop_signal.lock();
        if let Some(tx) = sig.take() {
            let _ = tx.send(());
        }
        eprintln!("[watch_folder] stopped");
    }

    /// Check if the service is currently active.
    pub fn is_active(&self) -> bool {
        self.inner.lock().active
    }

    /// Get the number of files detected since the service started.
    pub fn files_detected(&self) -> u64 {
        self.inner.lock().files_detected
    }
}

/// Background polling loop that checks for new video files.
async fn poll_loop(
    inner: Arc<Mutex<WatchFolderInner>>,
    app: AppHandle,
    mut stop_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));

    loop {
        tokio::select! {
            _ = &mut stop_rx => {
                break;
            }
            _ = interval.tick() => {
                let watch_path = {
                    let state = inner.lock();
                    if !state.active {
                        break;
                    }
                    state.watch_path.clone()
                };

                // Scan directory for video files
                let entries = match std::fs::read_dir(&watch_path) {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("[watch_folder] failed to read directory: {}", e);
                        continue;
                    }
                };

                for entry in entries.flatten() {
                    let path = entry.path();
                    if !is_video_file(&path) {
                        continue;
                    }

                    // Skip already-processed files
                    {
                        let state = inner.lock();
                        if state.seen_files.contains(&path) {
                            continue;
                        }
                    }

                    // Check if file modification time is after service start
                    let meta = match std::fs::metadata(&path) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };

                    let modified = match meta.modified() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };

                    let started_at = inner.lock().started_at;
                    if modified < started_at {
                        // File existed before service started — mark as seen and skip
                        inner.lock().seen_files.insert(path.clone());
                        continue;
                    }

                    let current_size = meta.len();

                    // Check stability: is the file still growing?
                    let is_stable = {
                        let mut state = inner.lock();
                        if let Some((last_size, first_seen)) = state.pending_files.get(&path) {
                            if current_size == *last_size {
                                // Size hasn't changed — check if stable long enough
                                first_seen.elapsed() >= Duration::from_secs(STABILIZE_SECS)
                            } else {
                                // Size changed — reset the stability timer
                                state.pending_files.insert(path.clone(), (current_size, Instant::now()));
                                false
                            }
                        } else {
                            // First time seeing this file — start tracking
                            state.pending_files.insert(path.clone(), (current_size, Instant::now()));
                            false
                        }
                    };

                    if is_stable {
                        // File is complete — emit event and mark as seen
                        let file_name = path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();

                        let payload = WatchFolderNewFile {
                            path: path.to_string_lossy().to_string(),
                            name: file_name.clone(),
                            size: current_size,
                        };

                        let _ = app.emit("watch-folder:new-file", &payload);
                        eprintln!("[watch_folder] new file detected: {} ({} bytes)", file_name, current_size);

                        let mut state = inner.lock();
                        state.seen_files.insert(path.clone());
                        state.pending_files.remove(&path);
                        state.files_detected += 1;
                    }
                }
            }
        }
    }

    // Mark as inactive when the loop exits
    inner.lock().active = false;
}

/// Check if a file has a supported video extension.
fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| VIDEO_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}
