//! Lossless trim service using Media Foundation stream copy.
//!
//! Uses MF Source Reader to read encoded H.264/audio samples from the input
//! and MF Sink Writer to write them to a new MP4 with adjusted timestamps.
//! This is a true passthrough mux (no decode/re-encode).
//!
//! Snaps the start time to the nearest preceding keyframe to avoid
//! re-encoding, writes to a temp file, then atomically moves to the output path.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Com::StructuredStorage::*;
use windows::Win32::System::Variant::VT_I8;

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
/// re-encoding. Uses MF Source Reader → Sink Writer passthrough (no quality loss).
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
    let actual_end = end;
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

    let input_owned = input.to_string();
    let temp_path_owned = temp_path.clone();
    let actual_start_copy = actual_start;
    let actual_end_copy = actual_end;

    // Run MF operations on a blocking thread
    let mf_result = tokio::task::spawn_blocking(move || {
        lossless_trim_mf(&input_owned, &temp_path_owned.to_string_lossy(), actual_start_copy, actual_end_copy)
    })
    .await
    .context("spawn_blocking failed")?;

    if let Err(e) = mf_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(e);
    }

    // Verify temp file was created and has content
    let temp_meta = std::fs::metadata(&temp_path)
        .context("Temp file was not created")?;
    if temp_meta.len() == 0 {
        let _ = std::fs::remove_file(&temp_path);
        anyhow::bail!("Media Foundation produced an empty output file");
    }

    // Atomic move: rename temp to final output
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

/// Perform lossless remux using Media Foundation Source Reader + Sink Writer.
/// Reads encoded samples (no decode) and writes them with adjusted timestamps.
fn lossless_trim_mf(input: &str, output: &str, start_sec: f64, end_sec: f64) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
            .context("MFStartup failed")?;

        let result = lossless_trim_mf_inner(input, output, start_sec, end_sec);

        let _ = MFShutdown();
        result
    }
}

unsafe fn lossless_trim_mf_inner(
    input: &str,
    output: &str,
    start_sec: f64,
    end_sec: f64,
) -> Result<()> {
    let wide_input: Vec<u16> = input.encode_utf16().chain(std::iter::once(0)).collect();
    let wide_output: Vec<u16> = output.encode_utf16().chain(std::iter::once(0)).collect();

    // Source Reader: no attributes = no decoding, reads compressed samples as-is
    let reader: IMFSourceReader =
        MFCreateSourceReaderFromURL(PCWSTR(wide_input.as_ptr()), None)
            .context("MFCreateSourceReaderFromURL failed")?;

    // Get native (compressed) media types
    let video_type: IMFMediaType = reader
        .GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)
        .context("No video stream")?;

    // Sink Writer: no attributes needed for passthrough
    let writer: IMFSinkWriter =
        MFCreateSinkWriterFromURL(PCWSTR(wide_output.as_ptr()), None, None)
            .context("MFCreateSinkWriterFromURL failed")?;

    // Add video stream with native compressed type
    let video_sink_idx = writer.AddStream(&video_type)
        .context("AddStream video failed")?;
    writer.SetInputMediaType(video_sink_idx, &video_type, None)
        .context("SetInputMediaType video failed")?;

    // Add audio stream if present
    let (has_audio, audio_sink_idx) = if let Ok(audio_type) = reader
        .GetNativeMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, 0)
    {
        if let Ok(idx) = writer.AddStream(&audio_type) {
            let _ = writer.SetInputMediaType(idx, &audio_type, None);
            (true, idx)
        } else {
            (false, 0)
        }
    } else {
        (false, 0)
    };

    // Seek to start position
    let start_100ns = (start_sec * 10_000_000.0) as i64;
    let end_100ns = (end_sec * 10_000_000.0) as i64;

    if start_100ns > 0 {
        let start_pv = make_propvariant_i64(start_100ns);
        reader.SetCurrentPosition(&GUID::zeroed(), &start_pv)
            .context("SetCurrentPosition failed")?;
    }

    writer.BeginWriting()
        .context("BeginWriting failed")?;

    // Read compressed samples and write to output with rebased timestamps
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

        if timestamp > end_100ns {
            break;
        }

        if let Some(ref s) = sample {
            let adjusted = timestamp - start_100ns;
            if adjusted < 0 {
                continue;
            }
            let _ = s.SetSampleTime(adjusted);

            let is_video = stream_index == MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32
                || stream_index == 0;

            if is_video {
                let _ = writer.WriteSample(video_sink_idx, s);
            } else if has_audio {
                let _ = writer.WriteSample(audio_sink_idx, s);
            }
        }
    }

    writer.Finalize()
        .context("Finalize failed")?;

    Ok(())
}

/// Create a PROPVARIANT containing an i64 value (VT_I8) for seeking.
unsafe fn make_propvariant_i64(value: i64) -> PROPVARIANT {
    let mut pv: PROPVARIANT = std::mem::zeroed();
    // Access the inner union to set vt and hVal
    (*pv.Anonymous.Anonymous).vt = VT_I8;
    (*pv.Anonymous.Anonymous).Anonymous.hVal = value;
    pv
}
