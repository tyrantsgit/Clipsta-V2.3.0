//! MP4 inspection service using Media Foundation Source Reader.
//!
//! Provides video metadata extraction and keyframe analysis without requiring
//! ffprobe. Uses IMFSourceReader to read stream properties and walk samples
//! for keyframe detection.

use anyhow::{Context, Result};
use serde::Serialize;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Com::StructuredStorage::*;

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

/// Inspect an MP4 file and return full metadata including keyframe timestamps.
pub async fn inspect_mp4(path: &str) -> Result<Mp4Info> {
    let path = path.to_string();
    tokio::task::spawn_blocking(move || inspect_mp4_sync(&path))
        .await
        .context("spawn_blocking failed")?
}

/// Synchronous Media Foundation inspection (must run on a blocking thread).
fn inspect_mp4_sync(path: &str) -> Result<Mp4Info> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
            .context("MFStartup failed")?;

        let result = inspect_mp4_inner(path);

        let _ = MFShutdown();
        result
    }
}

unsafe fn inspect_mp4_inner(path: &str) -> Result<Mp4Info> {
    // Create Source Reader
    let wide_path: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1)
        .context("MFCreateAttributes failed")?;
    let attrs = attrs.context("MFCreateAttributes returned None")?;

    // Enable hardware transforms
    attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
        .context("SetUINT32 failed")?;

    let reader: IMFSourceReader =
        MFCreateSourceReaderFromURL(PCWSTR(wide_path.as_ptr()), &attrs)
            .context("MFCreateSourceReaderFromURL failed")?;

    // Get duration from MF_PD_DURATION on the presentation descriptor
    let duration_100ns: i64 = reader
        .GetPresentationAttribute(
            MF_SOURCE_READER_MEDIASOURCE.0 as u32,
            &MF_PD_DURATION,
        )
        .ok()
        .and_then(|pv| PropVariantToInt64(&pv).ok())
        .unwrap_or(0);
    let duration = duration_100ns as f64 / 10_000_000.0;

    // Get video media type
    let video_type: IMFMediaType = reader
        .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
        .context("No video stream found")?;

    // Width and Height from MF_MT_FRAME_SIZE
    let frame_size = video_type
        .GetUINT64(&MF_MT_FRAME_SIZE)
        .unwrap_or(0);
    let width = (frame_size >> 32) as u32;
    let height = (frame_size & 0xFFFFFFFF) as u32;

    // FPS from MF_MT_FRAME_RATE
    let frame_rate = video_type
        .GetUINT64(&MF_MT_FRAME_RATE)
        .unwrap_or((30u64 << 32) | 1u64);
    let fps_num = (frame_rate >> 32) as f64;
    let fps_den = (frame_rate & 0xFFFFFFFF) as f64;
    let fps = if fps_den > 0.0 { fps_num / fps_den } else { 30.0 };

    // Codec from MF_MT_SUBTYPE
    let video_subtype: GUID = video_type
        .GetGUID(&MF_MT_SUBTYPE)
        .unwrap_or(MFVideoFormat_H264);
    let video_codec = guid_to_codec_name(&video_subtype);

    // Bitrate from MF_MT_AVG_BITRATE (may not be present)
    let bitrate = video_type
        .GetUINT32(&MF_MT_AVG_BITRATE)
        .unwrap_or(0) as u64;

    // Check for audio stream
    let has_audio = reader
        .GetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32)
        .is_ok();
    let audio_codec = if has_audio {
        reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32)
            .ok()
            .and_then(|t| t.GetGUID(&MF_MT_SUBTYPE).ok())
            .map(|g| audio_guid_to_codec_name(&g))
            .unwrap_or_default()
    } else {
        String::new()
    };

    // Walk samples for keyframes
    let keyframes = extract_keyframes(&reader)?;

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

/// Walk video samples and collect keyframe timestamps by checking MFSampleExtension_CleanPoint.
unsafe fn extract_keyframes(reader: &IMFSourceReader) -> Result<Vec<f64>> {
    let mut keyframes: Vec<f64> = Vec::new();
    let max_keyframes = 10000usize;

    loop {
        if keyframes.len() >= max_keyframes {
            break;
        }

        let mut flags: u32 = 0;
        let mut timestamp: i64 = 0;
        let mut sample: Option<IMFSample> = None;

        let hr = reader.ReadSample(
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
            0,
            None,
            Some(&mut flags),
            Some(&mut timestamp),
            Some(&mut sample),
        );

        if hr.is_err() {
            break;
        }

        // Check end of stream
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }

        // Check if this sample is a keyframe
        if let Some(ref s) = sample {
            let is_keyframe = s
                .GetUINT32(&MFSampleExtension_CleanPoint)
                .unwrap_or(0)
                != 0;
            if is_keyframe {
                let time_sec = timestamp as f64 / 10_000_000.0;
                keyframes.push(time_sec);
            }
        }
    }

    keyframes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Ok(keyframes)
}

/// Map a video subtype GUID to a human-readable codec name.
fn guid_to_codec_name(guid: &GUID) -> String {
    if *guid == MFVideoFormat_H264 {
        "h264".to_string()
    } else if *guid == MFVideoFormat_H265 {
        "hevc".to_string()
    } else if *guid == MFVideoFormat_VP90 {
        "vp9".to_string()
    } else if *guid == MFVideoFormat_AV1 {
        "av1".to_string()
    } else {
        format!("{:?}", guid)
    }
}

/// Map an audio subtype GUID to a human-readable codec name.
fn audio_guid_to_codec_name(guid: &GUID) -> String {
    if *guid == MFAudioFormat_AAC {
        "aac".to_string()
    } else if *guid == MFAudioFormat_MP3 {
        "mp3".to_string()
    } else if *guid == MFAudioFormat_PCM {
        "pcm".to_string()
    } else if *guid == MFAudioFormat_Float {
        "float".to_string()
    } else {
        format!("{:?}", guid)
    }
}

/// Find the nearest keyframe at or before the given time using binary search.
/// Returns 0.0 if no keyframe is found before the given time.
pub fn nearest_keyframe_before(keyframes: &[f64], time: f64) -> f64 {
    if keyframes.is_empty() {
        return 0.0;
    }

    match keyframes.binary_search_by(|k| k.partial_cmp(&time).unwrap_or(std::cmp::Ordering::Equal))
    {
        Ok(idx) => keyframes[idx],
        Err(idx) => {
            if idx == 0 {
                0.0
            } else {
                keyframes[idx - 1]
            }
        }
    }
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
}
