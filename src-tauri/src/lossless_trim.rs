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

    // Create Source Reader attributes
    let mut reader_attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut reader_attrs, 2)
        .context("MFCreateAttributes for reader failed")?;
    let reader_attrs = reader_attrs.context("reader attrs None")?;
    reader_attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
        .context("SetUINT32 reader failed")?;

    let reader: IMFSourceReader =
        MFCreateSourceReaderFromURL(PCWSTR(wide_input.as_ptr()), &reader_attrs)
            .context("MFCreateSourceReaderFromURL failed")?;

    // Create Sink Writer
    let mut writer_attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut writer_attrs, 1)
        .context("MFCreateAttributes for writer failed")?;
    let writer_attrs = writer_attrs.context("writer attrs None")?;
    writer_attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1).ok();

    let writer: IMFSinkWriter =
        MFCreateSinkWriterFromURL(PCWSTR(wide_output.as_ptr()), None, &writer_attrs)
            .context("MFCreateSinkWriterFromURL failed")?;

    // Configure video stream: passthrough (input type = output type)
    let video_type: IMFMediaType = reader
        .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
        .context("No video stream")?;

    let video_sink_idx = writer.AddStream(&video_type)
        .context("AddStream video failed")?;

    // Set input media type same as output for passthrough
    writer.SetInputMediaType(video_sink_idx, &video_type, None)
        .context("SetInputMediaType video failed")?;

    // Configure audio stream if present
    let (has_audio, audio_sink_idx) = if let Ok(audio_type) = reader
        .GetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32)
    {
        let idx = writer.AddStream(&audio_type)
            .context("AddStream audio failed")?;
        writer.SetInputMediaType(idx, &audio_type, None)
            .context("SetInputMediaType audio failed")?;
        (true, idx)
    } else {
        (false, 0)
    };

    // Seek Source Reader to the start position
    let start_100ns = (start_sec * 10_000_000.0) as i64;
    let end_100ns = (end_sec * 10_000_000.0) as i64;

    // Create PROPVARIANT with VT_I8 for seeking
    let start_pv = make_propvariant_i64(start_100ns);
    reader.SetCurrentPosition(&GUID::zeroed(), &start_pv)
        .context("SetCurrentPosition failed")?;

    // Begin writing
    writer.BeginWriting()
        .context("BeginWriting failed")?;

    // Read and write samples until end time
    let time_offset = start_100ns;

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

        // Stop if we've gone past the end time
        if timestamp > end_100ns {
            break;
        }

        if let Some(ref s) = sample {
            // Adjust timestamp to start from 0
            let adjusted_time = timestamp - time_offset;
            if adjusted_time < 0 {
                continue;
            }

            s.SetSampleTime(adjusted_time)?;

            // Determine which sink stream this belongs to
            let sink_idx = if stream_index == 0 {
                video_sink_idx
            } else if has_audio {
                audio_sink_idx
            } else {
                continue;
            };

            writer.WriteSample(sink_idx, s)
                .context("WriteSample failed")?;
        }
    }

    // Finalize
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
