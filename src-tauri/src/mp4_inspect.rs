//! MP4 inspection service using FFprobe.
//!
//! Provides video metadata extraction and keyframe analysis.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use tokio::process::Command;

/// Full MP4 metadata including keyframe positions.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Mp4Info {
    pub duration: f64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub video_codec: String,
    pub audio_codec: String,
    pub bitrate: u64,
    pub has_audio: bool,
    pub keyframes: Vec<f64>,
}

/// Resolve the path to ffprobe (same directory as ffmpeg, just different binary name).
pub fn find_ffprobe() -> String {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    if let Some(ref dir) = exe_dir {
        // Tauri bundles resources next to the exe
        let bundled = dir.join("ffprobe.exe");
        if bundled.exists() {
            return bundled.to_string_lossy().to_string();
        }
        // Also check resources subfolder
        let resources = dir.join("resources").join("ffprobe.exe");
        if resources.exists() {
            return resources.to_string_lossy().to_string();
        }
    }

    // Dev fallback: derive from ffmpeg path
    let dev_path = PathBuf::from("C:\\Users\\scott\\clipsta-win-V1\\bin\\ffprobe.exe");
    if dev_path.exists() {
        return dev_path.to_string_lossy().to_string();
    }

    // Fallback: try to find ffprobe next to ffmpeg
    let ffmpeg = crate::commands::find_ffmpeg_path();
    let ffmpeg_path = PathBuf::from(&ffmpeg);
    if let Some(parent) = ffmpeg_path.parent() {
        let probe = parent.join("ffprobe.exe");
        if probe.exists() {
            return probe.to_string_lossy().to_string();
        }
    }

    "ffprobe".to_string()
}

/// Inspect an MP4 file and return full metadata including keyframe timestamps.
pub async fn inspect_mp4(path: &str) -> Result<Mp4Info> {
    let ffprobe = find_ffprobe();

    // Get stream info (video + audio)
    let stream_output = Command::new(&ffprobe)
        .args([
            "-v", "quiet",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
            path,
        ])
        .output()
        .await
        .context("Failed to run ffprobe for stream info")?;

    if !stream_output.status.success() {
        let stderr = String::from_utf8_lossy(&stream_output.stderr);
        anyhow::bail!("ffprobe failed: {}", stderr);
    }

    let json_str = String::from_utf8_lossy(&stream_output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&json_str).context("Failed to parse ffprobe JSON output")?;

    // Extract video stream info
    let streams = json["streams"].as_array().context("No streams found")?;

    let video_stream = streams
        .iter()
        .find(|s| s["codec_type"].as_str() == Some("video"))
        .context("No video stream found")?;

    let audio_stream = streams
        .iter()
        .find(|s| s["codec_type"].as_str() == Some("audio"));

    let width = video_stream["width"].as_u64().unwrap_or(0) as u32;
    let height = video_stream["height"].as_u64().unwrap_or(0) as u32;
    let video_codec = video_stream["codec_name"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();

    // Parse FPS from r_frame_rate (e.g., "30000/1001")
    let fps = parse_frame_rate(
        video_stream["r_frame_rate"]
            .as_str()
            .unwrap_or("30/1"),
    );

    // Duration from format
    let duration = json["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    // Bitrate from format
    let bitrate = json["format"]["bit_rate"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    let has_audio = audio_stream.is_some();
    let audio_codec = audio_stream
        .and_then(|s| s["codec_name"].as_str())
        .unwrap_or("")
        .to_string();

    // Get keyframes
    let keyframes = get_keyframes(&ffprobe, path).await?;

    Ok(Mp4Info {
        duration,
        fps,
        width,
        height,
        video_codec,
        audio_codec,
        bitrate,
        has_audio,
        keyframes,
    })
}

/// Get keyframe timestamps from the video.
async fn get_keyframes(ffprobe: &str, path: &str) -> Result<Vec<f64>> {
    let output = Command::new(ffprobe)
        .args([
            "-v", "quiet",
            "-select_streams", "v:0",
            "-show_entries", "frame=pts_time,key_frame",
            "-of", "csv=p=0",
            path,
        ])
        .output()
        .await
        .context("Failed to run ffprobe for keyframes")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ffprobe keyframe extraction failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut keyframes = Vec::new();

    for line in stdout.lines() {
        // Format: "pts_time,key_frame" e.g., "1.234,1" or "1.234,0"
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() >= 2 {
            let is_keyframe = parts[1].trim() == "1";
            if is_keyframe {
                if let Ok(time) = parts[0].trim().parse::<f64>() {
                    keyframes.push(time);
                }
            }
        }
    }

    keyframes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Ok(keyframes)
}

/// Find the nearest keyframe at or before the given time using binary search.
/// Returns 0.0 if no keyframe is found before the given time.
pub fn nearest_keyframe_before(keyframes: &[f64], time: f64) -> f64 {
    if keyframes.is_empty() {
        return 0.0;
    }

    // Binary search for the rightmost keyframe <= time
    match keyframes.binary_search_by(|k| k.partial_cmp(&time).unwrap_or(std::cmp::Ordering::Equal))
    {
        Ok(idx) => keyframes[idx], // Exact match
        Err(idx) => {
            // idx is where `time` would be inserted
            if idx == 0 {
                0.0 // No keyframe before this time
            } else {
                keyframes[idx - 1]
            }
        }
    }
}

/// Parse a frame rate string like "30000/1001" or "30/1" into a float.
fn parse_frame_rate(rate: &str) -> f64 {
    if let Some((num, den)) = rate.split_once('/') {
        let n: f64 = num.parse().unwrap_or(30.0);
        let d: f64 = den.parse().unwrap_or(1.0);
        if d > 0.0 {
            return n / d;
        }
    }
    rate.parse().unwrap_or(30.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nearest_keyframe_before() {
        let keyframes = vec![0.0, 2.0, 4.0, 6.0, 8.0, 10.0];

        assert_eq!(nearest_keyframe_before(&keyframes, 5.0), 4.0);
        assert_eq!(nearest_keyframe_before(&keyframes, 4.0), 4.0);
        assert_eq!(nearest_keyframe_before(&keyframes, 0.5), 0.0);
        assert_eq!(nearest_keyframe_before(&keyframes, 11.0), 10.0);
        assert_eq!(nearest_keyframe_before(&[], 5.0), 0.0);
    }

    #[test]
    fn test_parse_frame_rate() {
        assert!((parse_frame_rate("30/1") - 30.0).abs() < 0.001);
        assert!((parse_frame_rate("30000/1001") - 29.97).abs() < 0.01);
        assert!((parse_frame_rate("60/1") - 60.0).abs() < 0.001);
    }
}
