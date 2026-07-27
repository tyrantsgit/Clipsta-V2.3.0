//! WASAPI desktop loopback + mic audio capture
//!
//! Captures audio via WASAPI loopback (system audio) + optional mic input.
//! Delivers mixed f32 stereo PCM chunks to a callback function for
//! integration with the Media Foundation SinkWriter.
//!
//! Key optimization: pre-allocated silence buffer avoids allocation every 10ms.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Format info parsed from WAVEFORMATEX.
#[derive(Clone, Copy)]
struct AudioFormat {
    channels: usize,
    bps: usize,
    sample_rate: u32,
    is_float: bool,
}

fn parse_format(ptr: *const WAVEFORMATEX) -> AudioFormat {
    unsafe {
        AudioFormat {
            channels: std::ptr::addr_of!((*ptr).nChannels).read_unaligned() as usize,
            bps: std::ptr::addr_of!((*ptr).wBitsPerSample).read_unaligned() as usize,
            sample_rate: std::ptr::addr_of!((*ptr).nSamplesPerSec).read_unaligned(),
            is_float: {
                let tag = std::ptr::addr_of!((*ptr).wFormatTag).read_unaligned();
                tag == 3 || tag == 0xFFFE
            },
        }
    }
}

pub struct WasapiCapture;

impl WasapiCapture {
    /// Capture audio and deliver chunks via callback.
    /// Callback receives interleaved f32 stereo samples at native sample rate.
    /// Uses pre-allocated silence buffer optimization for constant-rate audio delivery.
    pub fn capture_to_callback<F>(
        stop: Arc<AtomicBool>,
        mic_device: Option<String>,
        loopback_device: Option<String>,
        callback: F,
    ) -> Result<()>
    where
        F: Fn(&[f32]) + Send + 'static,
    {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("IMMDeviceEnumerator")?;

            let render_device = if let Some(ref id) = loopback_device {
                if id.is_empty() || id == "default" {
                    enumerator
                        .GetDefaultAudioEndpoint(eRender, eConsole)
                        .context("GetDefaultAudioEndpoint")?
                } else {
                    Self::find_render_device(&enumerator, id).unwrap_or_else(|_| {
                        enumerator
                            .GetDefaultAudioEndpoint(eRender, eConsole)
                            .unwrap()
                    })
                }
            } else {
                enumerator
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    .context("GetDefaultAudioEndpoint")?
            };

            let lb_client: IAudioClient = render_device
                .Activate(CLSCTX_ALL, None)
                .context("Activate loopback")?;
            let lb_fmt = lb_client.GetMixFormat().context("GetMixFormat")? as *const WAVEFORMATEX;
            lb_client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    2_000_000,
                    0,
                    lb_fmt,
                    None,
                )
                .context("Initialize loopback")?;
            let lb_capture: IAudioCaptureClient =
                lb_client.GetService().context("GetService loopback")?;

            let fmt = parse_format(lb_fmt);
            let bpf = fmt.channels * (fmt.bps / 8);
            let bps = fmt.bps / 8;

            // Optional mic setup
            let (mic_client, mic_capture, mic_fmt, mic_bpf) =
                if let Some(ref id) = mic_device {
                    if !id.is_empty() {
                        match Self::init_mic(&enumerator, id) {
                            Ok((c, cap, ptr)) => {
                                let mfmt = parse_format(ptr);
                                (Some(c), Some(cap), Some(mfmt), mfmt.channels * (mfmt.bps / 8))
                            }
                            Err(_) => (None, None, None, 0),
                        }
                    } else {
                        (None, None, None, 0)
                    }
                } else {
                    (None, None, None, 0)
                };

            let lb_event = CreateEventW(None, false, false, None).context("CreateEventW")?;
            lb_client
                .SetEventHandle(lb_event)
                .context("SetEventHandle")?;
            lb_client.Start().context("Loopback Start")?;

            let mic_event = mic_client.as_ref().and_then(|mc| {
                let e = CreateEventW(None, false, false, None).ok()?;
                mc.SetEventHandle(e).ok()?;
                mc.Start().ok()?;
                Some(e)
            });

            // Pre-allocated silence buffer (optimization: avoids alloc every 10ms iteration)
            // Max deficit per 10ms at 48kHz = ~480 frames = 960 stereo samples.
            // Allocate 2x headroom.
            let mut silence_buf: Vec<f32> = vec![0f32; 2048];
            let mut total_audio_frames_written: u64 = 0;
            let loop_start = std::time::Instant::now();

            while !stop.load(Ordering::SeqCst) {
                let wait = WaitForSingleObject(lb_event, 10);
                if stop.load(Ordering::SeqCst) {
                    break;
                }

                if wait == WAIT_OBJECT_0 {
                    // Capture loopback audio
                    loop {
                        let Ok(packet_frames) = lb_capture.GetNextPacketSize() else {
                            break;
                        };
                        if packet_frames == 0 {
                            break;
                        }
                        let mut ptr_raw = std::ptr::null_mut();
                        let mut frames = 0u32;
                        let mut flags = 0u32;
                        if lb_capture
                            .GetBuffer(&mut ptr_raw, &mut frames, &mut flags, None, None)
                            .is_err()
                        {
                            break;
                        }
                        if frames > 0 && !ptr_raw.is_null() {
                            let silent = (flags & 0x2) != 0;
                            let raw =
                                std::slice::from_raw_parts(ptr_raw, frames as usize * bpf);
                            let mut samples = if silent {
                                vec![0f32; frames as usize * 2]
                            } else {
                                to_f32_stereo(
                                    raw,
                                    frames as usize,
                                    fmt.channels,
                                    bps,
                                    fmt.is_float,
                                )
                            };

                            // Mix mic if available
                            if let (Some(ref mc_cap), Some(mic_f)) = (&mic_capture, mic_fmt) {
                                let mic_bps_val = mic_f.bps / 8;
                                loop {
                                    let Ok(mp) = mc_cap.GetNextPacketSize() else {
                                        break;
                                    };
                                    if mp == 0 {
                                        break;
                                    }
                                    let mut mp_raw = std::ptr::null_mut();
                                    let mut mf = 0u32;
                                    let mut mflags = 0u32;
                                    if mc_cap
                                        .GetBuffer(
                                            &mut mp_raw, &mut mf, &mut mflags, None, None,
                                        )
                                        .is_err()
                                    {
                                        break;
                                    }
                                    if mf > 0 && !mp_raw.is_null() {
                                        let msilent = (mflags & 0x2) != 0;
                                        let mraw = std::slice::from_raw_parts(
                                            mp_raw,
                                            mf as usize * mic_bpf,
                                        );
                                        let mic_samples = if msilent {
                                            vec![0f32; mf as usize * 2]
                                        } else {
                                            to_f32_stereo(
                                                mraw,
                                                mf as usize,
                                                mic_f.channels,
                                                mic_bps_val,
                                                mic_f.is_float,
                                            )
                                        };
                                        let mix_len = samples.len().min(mic_samples.len());
                                        for i in 0..mix_len {
                                            samples[i] =
                                                (samples[i] + mic_samples[i]).clamp(-1.0, 1.0);
                                        }
                                    }
                                    let _ = mc_cap.ReleaseBuffer(mf);
                                }
                            }

                            callback(&samples);
                            total_audio_frames_written += frames as u64;
                        }
                        let _ = lb_capture.ReleaseBuffer(frames);
                    }
                }

                // Fill silence to keep audio advancing at the constant sample rate
                // Also poll mic during silent periods so mic audio is never lost
                let elapsed_ms = loop_start.elapsed().as_millis() as u64;
                let expected_frames = elapsed_ms * fmt.sample_rate as u64 / 1000;
                if expected_frames > total_audio_frames_written {
                    let deficit = (expected_frames - total_audio_frames_written) as usize;
                    if deficit > 0 {
                        let needed = deficit * 2;
                        if silence_buf.len() < needed {
                            silence_buf.resize(needed, 0f32);
                        }
                        // Zero the silence buffer
                        for s in silence_buf[..needed].iter_mut() { *s = 0.0; }

                        // Mix in mic even during desktop-silent periods
                        if let (Some(ref mc_cap), Some(mic_f)) = (&mic_capture, mic_fmt) {
                            let mic_bps_val = mic_f.bps / 8;
                            loop {
                                let Ok(mp) = mc_cap.GetNextPacketSize() else { break; };
                                if mp == 0 { break; }
                                let mut mp_raw = std::ptr::null_mut();
                                let mut mf = 0u32;
                                let mut mflags = 0u32;
                                if mc_cap.GetBuffer(&mut mp_raw, &mut mf, &mut mflags, None, None).is_err() { break; }
                                if mf > 0 && !mp_raw.is_null() {
                                    let msilent = (mflags & 0x2) != 0;
                                    if !msilent {
                                        let mraw = std::slice::from_raw_parts(mp_raw, mf as usize * mic_bpf);
                                        let mic_samples = to_f32_stereo(mraw, mf as usize, mic_f.channels, mic_bps_val, mic_f.is_float);
                                        let mix_len = (needed).min(mic_samples.len());
                                        for i in 0..mix_len {
                                            silence_buf[i] = (silence_buf[i] + mic_samples[i]).clamp(-1.0, 1.0);
                                        }
                                    }
                                }
                                let _ = mc_cap.ReleaseBuffer(mf);
                            }
                        }

                        callback(&silence_buf[..needed]);
                        total_audio_frames_written += deficit as u64;
                    }
                }
            }

            let _ = lb_client.Stop();
            CloseHandle(lb_event).ok();
            if let (Some(ref mc), Some(me)) = (&mic_client, mic_event) {
                let _ = mc.Stop();
                CloseHandle(me).ok();
            }
        }
        Ok(())
    }

    /// Enumerate all WASAPI audio devices (render + capture).
    pub fn list_audio_devices() -> Result<Vec<serde_json::Value>> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("IMMDeviceEnumerator")?;
            let mut devices = Vec::new();

            let device_name = |dev: &IMMDevice| -> String {
                if let Ok(store) = dev.OpenPropertyStore(STGM_READ) {
                    let key = windows::Win32::Foundation::PROPERTYKEY {
                        fmtid: windows_core::GUID::from_u128(
                            0xa45c254e_df1c_4efd_8020_67d146a850e0,
                        ),
                        pid: 14,
                    };
                    if let Ok(val) = store.GetValue(&key) {
                        let s = val.to_string();
                        if !s.is_empty() {
                            return s;
                        }
                    }
                }
                String::new()
            };

            // Render endpoints (output)
            let render_col = enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE(1))
                .context("EnumAudioEndpoints render")?;
            let rcount = render_col.GetCount()?;
            for i in 0..rcount {
                if let Ok(dev) = render_col.Item(i) {
                    if let Ok(id) = dev.GetId() {
                        if let Ok(id_str) = id.to_string() {
                            let name = device_name(&dev);
                            devices.push(serde_json::json!({
                                "id": id_str, "name": name, "kind": "output"
                            }));
                        }
                    }
                }
            }

            // Capture endpoints (input)
            let cap_col = enumerator
                .EnumAudioEndpoints(eCapture, DEVICE_STATE(1))
                .context("EnumAudioEndpoints capture")?;
            let ccount = cap_col.GetCount()?;
            for i in 0..ccount {
                if let Ok(dev) = cap_col.Item(i) {
                    if let Ok(id) = dev.GetId() {
                        if let Ok(id_str) = id.to_string() {
                            let name = device_name(&dev);
                            devices.push(serde_json::json!({
                                "id": id_str, "name": name, "kind": "input"
                            }));
                        }
                    }
                }
            }

            Ok(devices)
        }
    }

    /// Get the system default audio device IDs (render output + capture input).
    pub fn get_default_devices() -> Result<serde_json::Value> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("IMMDeviceEnumerator")?;

            let default_output_id = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .ok()
                .and_then(|dev| dev.GetId().ok())
                .and_then(|id| id.to_string().ok())
                .unwrap_or_default();

            let default_input_id = enumerator
                .GetDefaultAudioEndpoint(eCapture, eConsole)
                .ok()
                .and_then(|dev| dev.GetId().ok())
                .and_then(|id| id.to_string().ok())
                .unwrap_or_default();

            Ok(serde_json::json!({
                "defaultOutputId": default_output_id,
                "defaultInputId": default_input_id,
            }))
        }
    }

    unsafe fn find_render_device(
        enumerator: &IMMDeviceEnumerator,
        device_id: &str,
    ) -> Result<IMMDevice> {
        let collection = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE(1))
            .context("EnumAudioEndpoints render")?;
        let count = collection.GetCount()?;
        for i in 0..count {
            let dev = collection.Item(i)?;
            let id_str = dev
                .GetId()
                .map(|id| id.to_string().unwrap_or_default())
                .unwrap_or_default();
            if id_str.contains(device_id) || device_id.contains(&id_str) {
                return Ok(dev);
            }
        }
        anyhow::bail!("render device not found matching: {}", device_id)
    }

    unsafe fn init_mic(
        enumerator: &IMMDeviceEnumerator,
        device_id: &str,
    ) -> Result<(IAudioClient, IAudioCaptureClient, *const WAVEFORMATEX)> {
        let device = if device_id == "default" {
            enumerator
                .GetDefaultAudioEndpoint(eCapture, eConsole)
                .context("GetDefaultAudioEndpoint mic")?
        } else {
            let collection = enumerator
                .EnumAudioEndpoints(eCapture, DEVICE_STATE(1))
                .context("EnumAudioEndpoints")?;
            let count = collection.GetCount()?;
            let mut found: Option<IMMDevice> = None;
            for i in 0..count {
                let dev = collection.Item(i)?;
                let id_str = dev
                    .GetId()
                    .map(|id| id.to_string().unwrap_or_default())
                    .unwrap_or_default();
                if id_str.contains(device_id) || device_id.contains(&id_str) {
                    found = Some(dev);
                    break;
                }
            }
            found.unwrap_or_else(|| {
                enumerator
                    .GetDefaultAudioEndpoint(eCapture, eConsole)
                    .unwrap()
            })
        };
        let c: IAudioClient = device.Activate(CLSCTX_ALL, None).context("Mic Activate")?;
        let fmt = c.GetMixFormat().context("Mic GetMixFormat")? as *const WAVEFORMATEX;
        c.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            2_000_000,
            0,
            fmt,
            None,
        )
        .context("Mic Initialize")?;
        let cap: IAudioCaptureClient = c.GetService().context("Mic GetService")?;
        Ok((c, cap, fmt))
    }
}

// ── PCM conversion helpers ────────────────────────────────────────────────────

fn to_f32_stereo(data: &[u8], frames: usize, ch: usize, bps: usize, float: bool) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames * 2);
    let bpf = ch * bps;
    for f in 0..frames {
        let b = f * bpf;
        if b + bps > data.len() {
            out.push(0.0);
            out.push(0.0);
            continue;
        }
        let l = read_sample(&data[b..], bps, float);
        let r = if ch >= 2 && b + bps * 2 <= data.len() {
            read_sample(&data[b + bps..], bps, float)
        } else {
            l
        };
        out.push(l);
        out.push(r);
    }
    out
}

fn read_sample(d: &[u8], bps: usize, float: bool) -> f32 {
    if d.len() < bps {
        return 0.0;
    }
    if float {
        match bps {
            4 => f32::from_le_bytes([d[0], d[1], d[2], d[3]]),
            8 => f64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]) as f32,
            _ => 0.0,
        }
    } else {
        match bps {
            2 => i16::from_le_bytes([d[0], d[1]]) as f32 / 32768.0,
            3 => {
                let v = (d[0] as i32) | ((d[1] as i32) << 8) | ((d[2] as i32) << 16);
                let v = if v & 0x800000 != 0 { v | !0xFFFFFF } else { v };
                v as f32 / 8388608.0
            }
            4 => i32::from_le_bytes([d[0], d[1], d[2], d[3]]) as f32 / 2147483648.0,
            _ => 0.0,
        }
    }
}
