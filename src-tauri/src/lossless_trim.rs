//! Lossless trim service using FFmpeg stream copy.
//!
//! Snaps the start time to the nearest preceding keyframe to avoid
//! re-encoding, writes to a temp file, then atomically moves to the output path.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use tokio::process::Command;

use crate::mp4_inspect::nearest_keyframe_before;

/// Result of a lossless trim operation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrimResult {
    pub output_path: String,
    pub requested_start: f64,
    pub actual_start: f64,
    pub requested_end: f64,
    pub actual_end: f64,
    pub duration: f64,
    pub extra_before: f64,
}

/// Perform a lossless trim of the input video.
///
/// The start time is snapped back to the nearest preceding keyframe to avoid
/// re-encoding. Uses `-c copy` for stream copy (no quality loss).
///
/// Writes to a temporary file first, then moves to the final output path.
pub async fn lossless_trim(
    input: &str,
    output: &str,
    start: f64,
    end: f64,
    keyframes: &[f64],
) -> Result<TrimResult> {
    // Snap start to nearest preceding keyframe
    let actual_start = nearest_keyframe_before(keyframes, start);
    let actual_end = end; // End doesn't need keyframe alignment for stream copy
    let duration = actual_end - actual_start;
    let extra_before = start - actual_start;

    if duration <= 0.0 {
        anyhow::bail!(
            "Invalid trim range: actual_start={}, actual_end={} (duration={})",
            actual_start,
            actual_end,
            duration
        );
    }

    // Find ffmpeg
    let ffmpeg = find_ffmpeg_for_trim();

    // Create temp file path in the same directory as output for atomic rename
    let output_path = PathBuf::from(output);
    let output_dir = output_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let temp_name = format!(
        ".clipsta_trim_{}.mp4",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    );
    let temp_path = output_dir.join(&temp_name);

    // Ensure output directory exists
    std::fs::create_dir_all(output_dir)
        .context("Failed to create output directory")?;

    // Run ffmpeg with stream copy
    let result = Command::new(&ffmpeg)
        .args([
            "-y",
            "-ss", &format!("{:.6}", actual_start),
            "-i", input,
            "-to", &format!("{:.6}", actual_end - actual_start), // duration relative to seek
            "-c", "copy",
            "-avoid_negative_ts", "make_zero",
            "-movflags", "+faststart",
            temp_path.to_string_lossy().as_ref(),
        ])
        .output()
        .await
        .context("Failed to execute ffmpeg for lossless trim")?;

    if !result.status.success() {
        // Clean up temp file on failure
        let _ = std::fs::remove_file(&temp_path);
        let stderr = String::from_utf8_lossy(&result.stderr);
        anyhow::bail!("FFmpeg trim failed: {}", stderr);
    }

    // Verify temp file was created and has content
    let temp_meta = std::fs::metadata(&temp_path)
        .context("Temp file was not created by ffmpeg")?;
    if temp_meta.len() == 0 {
        let _ = std::fs::remove_file(&temp_path);
        anyhow::bail!("FFmpeg produced an empty output file");
    }

    // Atomic move: rename temp to final output
    // On Windows, if the target exists we need to remove it first
    if output_path.exists() {
        std::fs::remove_file(&output_path)
            .context("Failed to remove existing output file")?;
    }
    std::fs::rename(&temp_path, &output_path)
        .context("Failed to move temp file to output path")?;

    Ok(TrimResult {
        output_path: output_path.to_string_lossy().to_string(),
        requested_start: start,
        actual_start,
        requested_end: end,
        actual_end,
        duration,
        extra_before,
    })
}

/// Resolve ffmpeg path for trim operations.
fn find_ffmpeg_for_trim() -> String {
    crate::commands::find_ffmpeg_path()
}
