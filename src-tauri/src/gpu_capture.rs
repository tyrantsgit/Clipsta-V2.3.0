//! Clipsta Lite GPU capture pipeline:
//! WGC → BGRA D3D11 texture → ID3D11VideoProcessor (scale + BGRA→NV12)
//! → ONE persistent async H.264 MFT → EncodedMediaRing (in-memory H.264 + PCM audio)
//! → keyframe-aligned slice on save → MF Sink Writer → MP4
//!
//! Key design:
//! - Single hardware encoder lives for the entire session (never recreated)
//! - Output: 1920×1088 (16-pixel aligned)
//! - Video Processor does scaling AND color conversion (BGRA→NV12)
//! - EncodedMediaRing holds encoded H.264 NALUs + raw PCM audio in memory
//! - On save: slice from ring at keyframe boundary, mux to MP4 via MF Sink Writer

use std::collections::VecDeque;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{Context as AnyhowContext, Result};
use parking_lot::Mutex;
use serde::Serialize;

use windows::core::{Interface, HSTRING};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::audio::WasapiCapture;

// ── Constants ─────────────────────────────────────────────────────────────────

const MF_VERSION: u32 = 0x0002_0070;
const AUDIO_SAMPLE_RATE: u32 = 48000;
const AUDIO_CHANNELS: u32 = 2;
const AUDIO_BITS_PER_SAMPLE: u32 = 16;
const AUDIO_BLOCK_ALIGN: u32 = AUDIO_CHANNELS * (AUDIO_BITS_PER_SAMPLE / 8);

/// Output dimensions: 1920×1088 (16-pixel aligned for hardware encoders)
const OUTPUT_WIDTH: u32 = 1920;
const OUTPUT_HEIGHT: u32 = 1088;

/// Maximum ring buffer duration in seconds (enough for longest possible clip)
const MAX_RING_SECONDS: u32 = 120;

/// NV12 pool size for video processor output
const NV12_POOL_SIZE: usize = 4;

fn pack_u64(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}



// ── D3D11 Device Creation ─────────────────────────────────────────────────────

/// Find the DXGI adapter that owns the given monitor (hybrid-GPU laptop fix).
unsafe fn find_adapter_for_monitor(hmon: HMONITOR) -> Option<IDXGIAdapter1> {
    let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
    let mut i = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(i) {
            Ok(a) => a,
            Err(_) => return None,
        };
        i += 1;
        let mut j = 0u32;
        loop {
            let output = match adapter.EnumOutputs(j) {
                Ok(o) => o,
                Err(_) => break,
            };
            j += 1;
            if let Ok(desc) = output.GetDesc() {
                if desc.Monitor == hmon {
                    return Some(adapter);
                }
            }
        }
    }
}

/// Create a D3D11 device with VIDEO_SUPPORT for VP and encoder.
unsafe fn create_d3d11_device(
    adapter: Option<&IDXGIAdapter1>,
) -> Result<(ID3D11Device, ID3D11DeviceContext, IDirect3DDevice)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;

    use windows::Win32::Foundation::HMODULE;

    let adapter_owned: Option<IDXGIAdapter> = match adapter {
        Some(a) => Some(a.cast()?),
        None => None,
    };
    let driver_type = if adapter_owned.is_some() {
        D3D_DRIVER_TYPE_UNKNOWN
    } else {
        D3D_DRIVER_TYPE_HARDWARE
    };

    D3D11CreateDevice(
        adapter_owned.as_ref(),
        driver_type,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        Some(&[
            D3D_FEATURE_LEVEL(0xb100),
            D3D_FEATURE_LEVEL(0xb000),
        ]),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )?;

    let device = device.context("D3D11 device")?;
    let context = context.context("D3D11 context")?;

    // Enable multithreaded access
    let mt: ID3D11Multithread = device.cast()?;
    let _ = mt.SetMultithreadProtected(true);

    // Create WinRT IDirect3DDevice for WGC
    let dxgi_device: IDXGIDevice = device.cast()?;
    let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)?;
    let winrt_device: IDirect3DDevice = inspectable.cast()?;

    Ok((device, context, winrt_device))
}



// ── ID3D11VideoProcessor: scale + BGRA→NV12 ──────────────────────────────────

struct VideoProcessorState {
    vp_device: ID3D11VideoDevice,
    vp_context: ID3D11VideoContext,
    vp_enum: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    src_width: u32,
    src_height: u32,
}

unsafe impl Send for VideoProcessorState {}
unsafe impl Sync for VideoProcessorState {}

impl VideoProcessorState {
    /// Create a video processor for BGRA→NV12 conversion + scaling.
    /// Pins source/dest rectangles explicitly (NVIDIA fix).
    unsafe fn new(
        device: &ID3D11Device,
        src_width: u32,
        src_height: u32,
        dst_width: u32,
        dst_height: u32,
    ) -> Result<Self> {
        let vp_device: ID3D11VideoDevice = device.cast()?;

        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            InputWidth: src_width,
            InputHeight: src_height,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            OutputWidth: dst_width,
            OutputHeight: dst_height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        let vp_enum = vp_device.CreateVideoProcessorEnumerator(&content_desc)?;
        let vp = vp_device.CreateVideoProcessor(&vp_enum, 0)?;

        let context: ID3D11DeviceContext = device.GetImmediateContext()?;
        let vp_context: ID3D11VideoContext = context.cast()?;

        // Pin source rectangle (NVIDIA fix: prevents auto-cropping)
        let src_rect = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: src_width as i32,
            bottom: src_height as i32,
        };
        vp_context.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&src_rect));

        // Pin destination rectangle
        let dst_rect = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: dst_width as i32,
            bottom: dst_height as i32,
        };
        vp_context.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&dst_rect));
        vp_context.VideoProcessorSetOutputTargetRect(&vp, true, Some(&dst_rect));

        Ok(Self {
            vp_device,
            vp_context,
            vp_enum,
            vp,
            src_width,
            src_height,
        })
    }

    /// Process one BGRA input texture → NV12 output texture.
    unsafe fn process(
        &self,
        input_tex: &ID3D11Texture2D,
        output_tex: &ID3D11Texture2D,
    ) -> Result<()> {
        // Create input view (BGRA)
        let input_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
        self.vp_device.CreateVideoProcessorInputView(
            input_tex,
            &self.vp_enum,
            &input_view_desc,
            Some(&mut input_view),
        )?;
        let input_view = input_view.context("VP input view")?;

        // Create output view (NV12)
        let output_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
        self.vp_device.CreateVideoProcessorOutputView(
            output_tex,
            &self.vp_enum,
            &output_view_desc,
            Some(&mut output_view),
        )?;
        let output_view = output_view.context("VP output view")?;

        // Build stream data
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
            ppFutureSurfaces: ptr::null_mut(),
            ..Default::default()
        };

        self.vp_context.VideoProcessorBlt(&self.vp, &output_view, 0, &[stream])?;
        Ok(())
    }

    /// Update source dimensions (when capture target resizes).
    unsafe fn update_source_size(&mut self, device: &ID3D11Device, new_w: u32, new_h: u32) -> Result<()> {
        if new_w == self.src_width && new_h == self.src_height {
            return Ok(());
        }
        // Recreate with new dimensions
        *self = Self::new(device, new_w, new_h, OUTPUT_WIDTH, OUTPUT_HEIGHT)?;
        Ok(())
    }
}

/// Create NV12 pool textures pre-filled with legal black (Y=16, U=V=128).
/// AMD fix: prevents green frame flash on first encode.
unsafe fn create_nv12_pool(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    width: u32,
    height: u32,
    count: usize,
) -> Result<Vec<ID3D11Texture2D>> {
    let mut pool = Vec::with_capacity(count);
    for _ in 0..count {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        let tex = tex.context("NV12 pool texture")?;

        // Pre-fill with legal black via staging texture
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..desc
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        let staging = staging.context("NV12 staging")?;

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))?;

        let pitch = mapped.RowPitch as usize;
        let ptr = mapped.pData as *mut u8;

        // Y plane: height rows, fill with 16 (legal black luma)
        for row in 0..height as usize {
            let row_ptr = ptr.add(row * pitch);
            std::ptr::write_bytes(row_ptr, 16u8, width as usize);
        }

        // UV plane: height/2 rows, fill with 128 (neutral chroma)
        let uv_offset = height as usize * pitch;
        for row in 0..(height / 2) as usize {
            let row_ptr = ptr.add(uv_offset + row * pitch);
            std::ptr::write_bytes(row_ptr, 128u8, width as usize);
        }

        context.Unmap(&staging, 0);
        context.CopyResource(&tex, &staging);

        pool.push(tex);
    }
    Ok(pool)
}



// ── Persistent H.264 Hardware Encoder (MFT) ───────────────────────────────────

#[allow(dead_code)]
struct PersistentEncoder {
    transform: IMFTransform,
    output_sample: IMFSample,
    input_type: IMFMediaType,
    output_type: IMFMediaType,
    use_cpu_input: bool,
}

unsafe impl Send for PersistentEncoder {}
unsafe impl Sync for PersistentEncoder {}

impl PersistentEncoder {
    /// Create ONE hardware H.264 encoder for the entire session.
    /// Sets ICodecAPI rate control (CBR + VBV) BEFORE SetOutputType.
    unsafe fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        // Activate hardware H.264 encoder MFT
        let out_type: IMFMediaType = MFCreateMediaType()?;
        out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
        out_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
        out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps * 1000)?;
        out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
        out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, 100)?; // High profile
        out_type.SetUINT32(&MF_MT_MPEG2_LEVEL, 42)?;

        // Find hardware encoder
        let flags = MFT_ENUM_FLAG(
            MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
        );
        let category = MFT_CATEGORY_VIDEO_ENCODER;

        let in_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let out_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };

        let mut activates_ptr: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count: u32 = 0;
        MFTEnumEx(
            category,
            flags,
            Some(&in_info),
            Some(&out_info),
            &mut activates_ptr,
            &mut count,
        )?;

        if count == 0 || activates_ptr.is_null() {
            // Try software encoders as fallback
            eprintln!("[PersistentEncoder] No hardware H.264 encoder found, trying software...");
            let sw_flags = MFT_ENUM_FLAG(
                MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_ASYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
            );
            MFTEnumEx(
                category,
                sw_flags,
                Some(&in_info),
                Some(&out_info),
                &mut activates_ptr,
                &mut count,
            )?;
            if count == 0 || activates_ptr.is_null() {
                anyhow::bail!("No H.264 encoder found (hardware or software)");
            }
        }

        let activates_slice = std::slice::from_raw_parts(activates_ptr, count as usize);
        let activate = activates_slice[0]
            .as_ref()
            .context("First encoder activate is None")?;
        let transform: IMFTransform = activate.ActivateObject()?;

        // Free the array AFTER we've activated the object
        CoTaskMemFree(Some(activates_ptr as *const _));

        // Unlock async MFT for synchronous use (required for NVIDIA/AMD hardware encoders)
        // Without this, ProcessInput/ProcessOutput will fail on async transforms.
        if let Ok(attrs) = transform.GetAttributes() {
            let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            eprintln!("[PersistentEncoder] async MFT unlocked for synchronous use");
        }

        // Set D3D11 device manager on the encoder for zero-copy NV12 input
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        let mut reset_token: u32 = 0;
        MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)?;
        let manager = manager.context("DXGI device manager for encoder")?;
        manager.ResetDevice(device, reset_token)?;

        // ProcessMessage expects the raw COM interface pointer as the param value
        let manager_ptr: *mut std::ffi::c_void = std::mem::transmute_copy(&manager);
        let set_result = transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager_ptr as usize);
        if let Err(e) = set_result {
            eprintln!("[PersistentEncoder] SET_D3D_MANAGER failed ({}), will use CPU NV12 buffers", e);
        } else {
            eprintln!("[PersistentEncoder] D3D manager set successfully — zero-copy NV12 input");
        }

        // Set ICodecAPI rate control BEFORE SetOutputType (constraint #6)
        if let Ok(codec_api) = transform.cast::<ICodecAPI>() {
            use windows::Win32::System::Variant::*;

            // Helper: create a VARIANT with a u32 value
            unsafe fn make_u32_variant(val: u32) -> VARIANT {
                let mut v = VARIANT::default();
                v.Anonymous.Anonymous = std::mem::ManuallyDrop::new(VARIANT_0_0 {
                    vt: VT_UI4,
                    Anonymous: VARIANT_0_0_0 { ulVal: val },
                    ..Default::default()
                });
                v
            }

            unsafe fn make_bool_variant(val: bool) -> VARIANT {
                let mut v = VARIANT::default();
                v.Anonymous.Anonymous = std::mem::ManuallyDrop::new(VARIANT_0_0 {
                    vt: VT_BOOL,
                    Anonymous: VARIANT_0_0_0 {
                        boolVal: windows::Win32::Foundation::VARIANT_BOOL(if val { -1i16 } else { 0i16 }),
                    },
                    ..Default::default()
                });
                v
            }

            // CBR rate control (eAVEncCommonRateControlMode = 2)
            let val = make_u32_variant(2);
            let _ = codec_api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &val);

            // Mean bitrate
            let val = make_u32_variant(bitrate_kbps * 1000);
            let _ = codec_api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &val);

            // VBV buffer size = 2x bitrate (constraint #11)
            let val = make_u32_variant(bitrate_kbps * 1000 * 2);
            let _ = codec_api.SetValue(&CODECAPI_AVEncCommonBufferSize, &val);

            // Low latency mode
            let val = make_bool_variant(true);
            let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &val);
        }

        // Now set output type (H.264)
        transform.SetOutputType(0, &out_type, 0)?;

        // Set input type (NV12)
        let in_type: IMFMediaType = MFCreateMediaType()?;
        in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
        in_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
        in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;

        // Try to set input type. If it fails (common when D3D manager isn't accepted),
        // enumerate the encoder's preferred input types and use the first NV12 one.
        let set_input_result = transform.SetInputType(0, &in_type, 0);
        let use_cpu_input = if let Err(e) = set_input_result {
            eprintln!("[PersistentEncoder] SetInputType with custom NV12 failed ({}), trying encoder's preferred type", e);
            // Try to get the encoder's preferred input type
            let mut found = false;
            for i in 0..20u32 {
                match transform.GetInputAvailableType(0, i) {
                    Ok(preferred) => {
                        if let Ok(sub) = preferred.GetGUID(&MF_MT_SUBTYPE) {
                            if sub == MFVideoFormat_NV12 {
                                eprintln!("[PersistentEncoder] using encoder's preferred NV12 input type (index {})", i);
                                transform.SetInputType(0, &preferred, 0)?;
                                found = true;
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            if !found {
                anyhow::bail!("Encoder does not accept NV12 input");
            }
            true // CPU input mode (we'll map textures to memory)
        } else {
            false
        };

        eprintln!("[PersistentEncoder] input type set, cpu_input={}", use_cpu_input);

        // Notify encoder it's ready
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

        // Pre-allocate output sample with buffer
        let output_sample: IMFSample = MFCreateSample()?;
        let out_buf: IMFMediaBuffer = MFCreateMemoryBuffer(1024 * 1024)?; // 1MB initial
        output_sample.AddBuffer(&out_buf)?;

        Ok(Self {
            transform,
            output_sample,
            input_type: in_type,
            output_type: out_type,
            use_cpu_input,
        })
    }

    /// Feed one NV12 texture to the encoder, return encoded H.264 NALUs (if any).
    unsafe fn encode_frame(
        &self,
        nv12_texture: &ID3D11Texture2D,
        pts_100ns: i64,
        duration_100ns: i64,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Result<Vec<EncodedFrame>> {
        let sample: IMFSample = MFCreateSample()?;

        if self.use_cpu_input {
            // CPU path: map NV12 texture to system memory
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
            let staging = staging.context("NV12 staging texture")?;

            context.CopyResource(&staging, nv12_texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

            // NV12: Y plane = width*height bytes, UV plane = width*height/2 bytes
            let nv12_size = (width * height * 3 / 2) as usize;
            let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(nv12_size as u32)?;
            let mut p: *mut u8 = ptr::null_mut();
            buffer.Lock(&mut p, None, None)?;

            let src = mapped.pData as *const u8;
            let row_pitch = mapped.RowPitch as usize;

            // Copy Y plane (height rows)
            for row in 0..height as usize {
                ptr::copy_nonoverlapping(
                    src.add(row * row_pitch),
                    p.add(row * width as usize),
                    width as usize,
                );
            }
            // Copy UV plane (height/2 rows)
            let uv_offset_src = height as usize * row_pitch;
            let uv_offset_dst = (width * height) as usize;
            for row in 0..(height / 2) as usize {
                ptr::copy_nonoverlapping(
                    src.add(uv_offset_src + row * row_pitch),
                    p.add(uv_offset_dst + row * width as usize),
                    width as usize,
                );
            }

            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12_size as u32)?;
            context.Unmap(&staging, 0);

            sample.AddBuffer(&buffer)?;
        } else {
            // GPU path: DXGI surface buffer (zero-copy)
            let buffer: IMFMediaBuffer = MFCreateDXGISurfaceBuffer(
                &ID3D11Texture2D::IID,
                nv12_texture,
                0,
                false,
            )?;
            sample.AddBuffer(&buffer)?;
        }

        sample.SetSampleTime(pts_100ns)?;
        sample.SetSampleDuration(duration_100ns)?;

        // Feed input
        let input_result = self.transform.ProcessInput(0, &sample, 0);
        if input_result.is_err() {
            // MF_E_NOTACCEPTING means we need to drain output first
            let mut frames = self.drain_output()?;
            // Retry input
            self.transform.ProcessInput(0, &sample, 0)?;
            frames.extend(self.drain_output()?);
            return Ok(frames);
        }

        // Try to get output
        self.drain_output()
    }

    /// Drain all available encoded output from the MFT.
    unsafe fn drain_output(&self) -> Result<Vec<EncodedFrame>> {
        let mut frames = Vec::new();

        loop {
            let out_sample: IMFSample = MFCreateSample()?;
            let out_buf: IMFMediaBuffer = MFCreateMemoryBuffer(512 * 1024)?;
            out_sample.AddBuffer(&out_buf)?;

            let output_buffer = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(Some(out_sample.clone())),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };

            let mut status: u32 = 0;
            let hr = self.transform.ProcessOutput(
                0,
                &mut [output_buffer],
                &mut status,
            );

            match hr {
                Ok(()) => {
                    // Extract the encoded data from out_sample
                    let pts = out_sample.GetSampleTime().unwrap_or(0);
                    let dur = out_sample.GetSampleDuration().unwrap_or(0);
                    let buf = out_sample.GetBufferByIndex(0)?;
                    let mut p: *mut u8 = ptr::null_mut();
                    let mut len: u32 = 0;
                    buf.Lock(&mut p, None, Some(&mut len))?;
                    let data = std::slice::from_raw_parts(p, len as usize).to_vec();
                    buf.Unlock()?;

                    let is_keyframe = is_nalu_keyframe(&data);

                    frames.push(EncodedFrame {
                        data,
                        pts_100ns: pts,
                        duration_100ns: dur,
                        is_keyframe,
                    });
                }
                Err(_) => {
                    // MF_E_TRANSFORM_NEED_MORE_INPUT or other → stop draining
                    break;
                }
            }
        }

        Ok(frames)
    }

    /// Flush the encoder (call on session end).
    unsafe fn flush(&self) -> Result<Vec<EncodedFrame>> {
        self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        // Drain remaining output
        let mut frames = Vec::new();
        for _ in 0..100 {
            let batch = self.drain_output()?;
            if batch.is_empty() {
                break;
            }
            frames.extend(batch);
        }
        Ok(frames)
    }
}

/// Detect keyframes by parsing NAL unit headers.
/// Look for IDR (type 5) or SPS (type 7) — NOT MFSampleExtension_CleanPoint.
fn is_nalu_keyframe(data: &[u8]) -> bool {
    if data.len() < 5 {
        return false;
    }
    // Search for start codes (0x00 0x00 0x01 or 0x00 0x00 0x00 0x01)
    let mut i = 0;
    while i < data.len().saturating_sub(4) {
        let is_3byte_start = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        let is_4byte_start =
            data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1;

        if is_4byte_start {
            let nal_type = data[i + 4] & 0x1F;
            if nal_type == 5 || nal_type == 7 {
                return true;
            }
            i += 4;
        } else if is_3byte_start {
            let nal_type = data[i + 3] & 0x1F;
            if nal_type == 5 || nal_type == 7 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}



// ── EncodedMediaRing: circular buffer of encoded H.264 + PCM audio ────────────

#[derive(Clone)]
struct EncodedFrame {
    data: Vec<u8>,
    pts_100ns: i64,
    duration_100ns: i64,
    is_keyframe: bool,
}

#[derive(Clone)]
struct AudioChunk {
    /// Interleaved f32 stereo samples (will be PCM i16 at mux time)
    data: Vec<f32>,
    pts_100ns: i64,
    duration_100ns: i64,
}

/// Ring buffer holding encoded H.264 frames + PCM audio chunks.
/// Maintains a keyframe index for fast slicing.
struct EncodedMediaRing {
    video_frames: VecDeque<EncodedFrame>,
    audio_chunks: VecDeque<AudioChunk>,
    /// Indices into video_frames that are keyframes (for fast seeking)
    keyframe_indices: VecDeque<usize>,
    /// Running offset: total frames ever pushed (to convert absolute index → deque index)
    frames_pushed: usize,
    max_duration_100ns: i64,
}

unsafe impl Send for EncodedMediaRing {}
unsafe impl Sync for EncodedMediaRing {}

impl EncodedMediaRing {
    fn new(max_seconds: u32) -> Self {
        Self {
            video_frames: VecDeque::with_capacity(max_seconds as usize * 60),
            audio_chunks: VecDeque::with_capacity(max_seconds as usize * 50),
            keyframe_indices: VecDeque::new(),
            frames_pushed: 0,
            max_duration_100ns: max_seconds as i64 * 10_000_000,
        }
    }

    /// Push an encoded video frame into the ring.
    fn push_video(&mut self, frame: EncodedFrame) {
        if frame.is_keyframe {
            self.keyframe_indices.push_back(self.frames_pushed);
        }
        self.video_frames.push_back(frame);
        self.frames_pushed += 1;
        self.prune();
    }

    /// Push a PCM audio chunk into the ring.
    fn push_audio(&mut self, chunk: AudioChunk) {
        self.audio_chunks.push_back(chunk);
        self.prune_audio();
    }

    /// Remove old frames that exceed max buffer duration.
    fn prune(&mut self) {
        while self.video_frames.len() > 2 {
            let newest_pts = self.video_frames.back().map(|f| f.pts_100ns).unwrap_or(0);
            let oldest_pts = self.video_frames.front().map(|f| f.pts_100ns).unwrap_or(0);
            if newest_pts - oldest_pts > self.max_duration_100ns {
                self.video_frames.pop_front();
                let base = self.frames_pushed - self.video_frames.len() - 1;
                // Remove keyframe indices that fell off
                while let Some(&ki) = self.keyframe_indices.front() {
                    if ki <= base {
                        self.keyframe_indices.pop_front();
                    } else {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn prune_audio(&mut self) {
        if self.video_frames.is_empty() {
            return;
        }
        let oldest_video_pts = self.video_frames.front().map(|f| f.pts_100ns).unwrap_or(0);
        while let Some(front) = self.audio_chunks.front() {
            if front.pts_100ns + front.duration_100ns < oldest_video_pts {
                self.audio_chunks.pop_front();
            } else {
                break;
            }
        }
    }

    /// Find the keyframe at or before (newest_pts - requested_seconds).
    /// Returns the deque-local index of that keyframe.
    fn find_slice_start(&self, seconds: u32) -> Option<usize> {
        if self.video_frames.is_empty() {
            return None;
        }
        let newest_pts = self.video_frames.back()?.pts_100ns;
        let target_pts = newest_pts - (seconds as i64 * 10_000_000);

        let base_offset = self.frames_pushed - self.video_frames.len();

        // Walk keyframe indices backwards to find the one at or before target_pts
        let mut best: Option<usize> = None;
        for &abs_idx in self.keyframe_indices.iter().rev() {
            let local_idx = abs_idx.saturating_sub(base_offset);
            if local_idx >= self.video_frames.len() {
                continue;
            }
            let frame = &self.video_frames[local_idx];
            if frame.pts_100ns <= target_pts {
                best = Some(local_idx);
                break;
            }
            best = Some(local_idx); // keep updating; we want the earliest one at or before target
        }

        // If no keyframe before target, use the earliest available keyframe
        if best.is_none() {
            for &abs_idx in self.keyframe_indices.iter() {
                let local_idx = abs_idx.saturating_sub(base_offset);
                if local_idx < self.video_frames.len() {
                    best = Some(local_idx);
                    break;
                }
            }
        }

        best
    }

    /// Slice video frames from start_idx to end.
    fn slice_video(&self, start_idx: usize) -> Vec<EncodedFrame> {
        self.video_frames.iter().skip(start_idx).cloned().collect()
    }

    /// Slice audio chunks that overlap the given PTS range.
    fn slice_audio(&self, start_pts: i64, end_pts: i64) -> Vec<AudioChunk> {
        self.audio_chunks
            .iter()
            .filter(|c| c.pts_100ns + c.duration_100ns > start_pts && c.pts_100ns < end_pts)
            .cloned()
            .collect()
    }

    #[allow(dead_code)]
    fn newest_pts(&self) -> i64 {
        self.video_frames.back().map(|f| f.pts_100ns).unwrap_or(0)
    }

    #[allow(dead_code)]
    fn oldest_pts(&self) -> i64 {
        self.video_frames.front().map(|f| f.pts_100ns).unwrap_or(0)
    }

    #[allow(dead_code)]
    fn duration_secs(&self) -> f64 {
        (self.newest_pts() - self.oldest_pts()) as f64 / 10_000_000.0
    }
}



// ── MP4 Muxer: MF Sink Writer for save operation ──────────────────────────────

/// Mux sliced H.264 frames + PCM audio → MP4 file using MF Sink Writer.
/// Audio is AAC-encoded at mux time (constraint #10).
unsafe fn mux_to_mp4(
    output_path: &str,
    video_frames: &[EncodedFrame],
    audio_chunks: &[AudioChunk],
    fps: u32,
) -> Result<()> {
    if video_frames.is_empty() {
        anyhow::bail!("No video frames to mux");
    }

    let mut attr: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attr, 2)?;
    let attr = attr.context("mux attributes")?;
    attr.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
    attr.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;

    let path: HSTRING = output_path.into();
    let writer: IMFSinkWriter = MFCreateSinkWriterFromURL(&path, None, &attr)?;

    // Video stream: passthrough H.264 (already encoded)
    let vout: IMFMediaType = MFCreateMediaType()?;
    vout.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    vout.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
    vout.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(OUTPUT_WIDTH, OUTPUT_HEIGHT))?;
    vout.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
    vout.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    vout.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
    vout.SetUINT32(&MF_MT_MPEG2_PROFILE, 100)?;
    vout.SetUINT32(&MF_MT_MPEG2_LEVEL, 42)?;
    let video_stream = writer.AddStream(&vout)?;

    // For passthrough: input type == output type (H.264)
    let vin: IMFMediaType = MFCreateMediaType()?;
    vin.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    vin.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
    vin.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(OUTPUT_WIDTH, OUTPUT_HEIGHT))?;
    vin.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
    vin.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    vin.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
    writer.SetInputMediaType(video_stream, &vin, None)?;

    // Audio stream: PCM input → AAC output (encoded at mux time)
    let has_audio = !audio_chunks.is_empty();
    let audio_stream = if has_audio {
        let aout: IMFMediaType = MFCreateMediaType()?;
        aout.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        aout.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        aout.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
        aout.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS)?;
        aout.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, AUDIO_BITS_PER_SAMPLE)?;
        aout.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24000)?;
        let idx = writer.AddStream(&aout)?;

        let ain: IMFMediaType = MFCreateMediaType()?;
        ain.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        ain.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        ain.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, AUDIO_BITS_PER_SAMPLE)?;
        ain.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
        ain.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS)?;
        ain.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, AUDIO_BLOCK_ALIGN)?;
        ain.SetUINT32(
            &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
            AUDIO_SAMPLE_RATE * AUDIO_BLOCK_ALIGN,
        )?;
        writer.SetInputMediaType(idx, &ain, None)?;
        Some(idx)
    } else {
        None
    };

    writer.BeginWriting()?;

    // Rebase PTS so clip starts at 0
    let base_pts = video_frames[0].pts_100ns;

    // Write video frames
    for frame in video_frames {
        let buf: IMFMediaBuffer = MFCreateMemoryBuffer(frame.data.len() as u32)?;
        let mut p: *mut u8 = ptr::null_mut();
        buf.Lock(&mut p, None, None)?;
        ptr::copy_nonoverlapping(frame.data.as_ptr(), p, frame.data.len());
        buf.Unlock()?;
        buf.SetCurrentLength(frame.data.len() as u32)?;

        let sample: IMFSample = MFCreateSample()?;
        sample.AddBuffer(&buf)?;
        sample.SetSampleTime(frame.pts_100ns - base_pts)?;
        sample.SetSampleDuration(frame.duration_100ns)?;

        if frame.is_keyframe {
            sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
        }

        writer.WriteSample(video_stream, &sample)?;
    }

    // Write audio (PCM → AAC conversion done by Sink Writer)
    if let Some(audio_idx) = audio_stream {
        for chunk in audio_chunks {
            let i16_buf: Vec<i16> = chunk
                .data
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect();
            let byte_len = (i16_buf.len() * 2) as u32;
            let buf: IMFMediaBuffer = MFCreateMemoryBuffer(byte_len)?;
            let mut p: *mut u8 = ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            ptr::copy_nonoverlapping(i16_buf.as_ptr() as *const u8, p, byte_len as usize);
            buf.Unlock()?;
            buf.SetCurrentLength(byte_len)?;

            let sample: IMFSample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime((chunk.pts_100ns - base_pts).max(0))?;
            sample.SetSampleDuration(chunk.duration_100ns)?;
            writer.WriteSample(audio_idx, &sample)?;
        }
    }

    writer.Finalize()?;
    Ok(())
}



// ── Public API: CaptureSession ────────────────────────────────────────────────

#[derive(Clone, Serialize)]
pub struct CompletedSegment {
    pub path: String,
    pub index: u32,
    pub start_pts: f64,
    pub end_pts: f64,
    pub duration: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaptureReadyInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub segment_dir: String,
}

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub source_id: Option<String>,
    pub fps: u32,
    pub no_audio: bool,
    pub mic_device: Option<String>,
    pub loopback_device: Option<String>,
    pub target_width: Option<u32>,
    pub target_height: Option<u32>,
    pub bitrate_kbps: u32,
    pub segment_duration: u32,
    pub buffer_duration: u32,
    pub segment_dir: PathBuf,
}

pub struct CaptureSession {
    pub is_recording: Arc<AtomicBool>,
    pub is_saving: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    /// Completed saved clips (not buffer segments)
    saved_clips: Arc<Mutex<Vec<CompletedSegment>>>,
    segment_dir: Arc<Mutex<Option<PathBuf>>>,
    recording_start: Arc<Mutex<Option<std::time::Instant>>>,
    audio_file: Arc<Mutex<Option<String>>>,
    /// Shared ring buffer for the capture pipeline
    ring: Arc<Mutex<EncodedMediaRing>>,
    /// FPS for the current session (needed for muxing)
    session_fps: Arc<AtomicU32>,
    /// Clip counter for indexing saved clips
    clip_counter: Arc<AtomicU32>,
}

impl Default for CaptureSession {
    fn default() -> Self {
        Self {
            is_recording: Arc::new(AtomicBool::new(false)),
            is_saving: Arc::new(AtomicBool::new(false)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            saved_clips: Arc::new(Mutex::new(Vec::new())),
            segment_dir: Arc::new(Mutex::new(None)),
            recording_start: Arc::new(Mutex::new(None)),
            audio_file: Arc::new(Mutex::new(None)),
            ring: Arc::new(Mutex::new(EncodedMediaRing::new(MAX_RING_SECONDS))),
            session_fps: Arc::new(AtomicU32::new(60)),
            clip_counter: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl CaptureSession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(
        &self,
        opts: CaptureOptions,
        _on_segment: Box<dyn Fn(CompletedSegment) + Send + 'static>,
    ) -> Result<CaptureReadyInfo> {
        if self.is_recording.load(Ordering::Relaxed) {
            anyhow::bail!("Already recording");
        }
        if self.is_saving.load(Ordering::Relaxed) {
            anyhow::bail!("Save in progress");
        }

        self.stop_flag.store(false, Ordering::SeqCst);
        *self.saved_clips.lock() = Vec::new();
        *self.segment_dir.lock() = Some(opts.segment_dir.clone());
        self.session_fps.store(opts.fps, Ordering::SeqCst);

        // Reset ring buffer with appropriate duration
        *self.ring.lock() = EncodedMediaRing::new(opts.buffer_duration.max(MAX_RING_SECONDS));

        let stop = self.stop_flag.clone();
        let is_recording = self.is_recording.clone();
        let ring = self.ring.clone();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<CaptureReadyInfo>>();

        thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let result = run_gpu_capture(opts, stop.clone(), ring, ready_tx.clone());
            match result {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("[gpu_capture] pipeline error: {}", e);
                    // If ready_tx hasn't been consumed yet, send the error back
                    let _ = ready_tx.send(Err(anyhow::anyhow!("{}", e)));
                }
            }
            is_recording.store(false, Ordering::SeqCst);
        });

        let ready = ready_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|_| anyhow::anyhow!("Capture start timeout (10s)"))?;

        let info = ready?;
        self.is_recording.store(true, Ordering::SeqCst);
        *self.recording_start.lock() = Some(std::time::Instant::now());
        *self.audio_file.lock() = None;
        Ok(info)
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.is_recording.store(false, Ordering::SeqCst);
    }

    /// Returns completed SAVED clips (not buffer segments).
    pub fn get_segments(&self) -> Vec<CompletedSegment> {
        self.saved_clips.lock().clone()
    }

    pub fn get_segment_dir(&self) -> Option<PathBuf> {
        self.segment_dir.lock().clone()
    }

    pub fn cleanup(&self) {
        if let Some(dir) = self.segment_dir.lock().take() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        *self.saved_clips.lock() = Vec::new();
        *self.ring.lock() = EncodedMediaRing::new(MAX_RING_SECONDS);
    }

    pub fn get_audio_file(&self) -> Option<String> {
        self.audio_file.lock().clone()
    }

    pub fn elapsed_secs(&self) -> Option<f64> {
        self.recording_start.lock().map(|start| start.elapsed().as_secs_f64())
    }

    pub fn finalize_pending_segments(&self) {
        // In the ring-buffer architecture, there's nothing to finalize.
        // The ring always holds the latest encoded data.
    }

    /// Save a clip: slice from the ring buffer at keyframe boundary, mux to MP4.
    /// This is the core "instant replay" operation.
    pub fn save_clip(&self, seconds: u32, output_path: &str) -> Result<String> {
        if !self.is_recording.load(Ordering::Relaxed) {
            anyhow::bail!("Not recording — cannot save clip");
        }

        let fps = self.session_fps.load(Ordering::Relaxed);

        // Snapshot the ring under lock
        let (video_frames, audio_chunks) = {
            let ring = self.ring.lock();

            let start_idx = ring
                .find_slice_start(seconds)
                .ok_or_else(|| anyhow::anyhow!("No keyframe found in ring buffer"))?;

            let video = ring.slice_video(start_idx);
            if video.is_empty() {
                anyhow::bail!("No video frames available for clip");
            }

            let start_pts = video[0].pts_100ns;
            let end_pts = video.last().map(|f| f.pts_100ns + f.duration_100ns).unwrap_or(start_pts);
            let audio = ring.slice_audio(start_pts, end_pts);

            (video, audio)
        };

        eprintln!(
            "[gpu_capture] save_clip: {} video frames, {} audio chunks, output: {}",
            video_frames.len(),
            audio_chunks.len(),
            output_path
        );

        // Mux to MP4 (this does the AAC encoding of PCM audio at mux time)
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            let result = mux_to_mp4(output_path, &video_frames, &audio_chunks, fps);
            MFShutdown()?;
            result?;
        }

        // Track the saved clip
        let clip_idx = self.clip_counter.fetch_add(1, Ordering::Relaxed);
        let duration = video_frames.last().map(|f| f.pts_100ns + f.duration_100ns).unwrap_or(0)
            - video_frames[0].pts_100ns;
        let seg = CompletedSegment {
            path: output_path.to_string(),
            index: clip_idx,
            start_pts: video_frames[0].pts_100ns as f64 / 10_000_000.0,
            end_pts: video_frames.last().map(|f| (f.pts_100ns + f.duration_100ns) as f64 / 10_000_000.0).unwrap_or(0.0),
            duration: duration as f64 / 10_000_000.0,
        };
        self.saved_clips.lock().push(seg);

        Ok(output_path.to_string())
    }
}



// ── WGC Capture Item Helpers ──────────────────────────────────────────────────

unsafe fn capture_item_from_monitor(hmon: HMONITOR) -> Result<GraphicsCaptureItem> {
    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = interop.CreateForMonitor(hmon)?;
    Ok(item)
}

unsafe fn capture_item_from_window(hwnd: HWND) -> Result<GraphicsCaptureItem> {
    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = interop.CreateForWindow(hwnd)?;
    Ok(item)
}

// ── GPU Capture Loop ──────────────────────────────────────────────────────────

fn run_gpu_capture(
    opts: CaptureOptions,
    stop: Arc<AtomicBool>,
    ring: Arc<Mutex<EncodedMediaRing>>,
    ready_tx: std::sync::mpsc::Sender<Result<CaptureReadyInfo>>,
) -> Result<()> {
    unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }

    // Resolve target window/monitor
    let target_hwnd: Option<HWND> = match opts.source_id.as_deref() {
        Some(id) if id.starts_with("hwnd:") => {
            let v: usize = id[5..].parse().map_err(|e| anyhow::anyhow!("bad hwnd: {}", e))?;
            Some(HWND(v as *mut _))
        }
        _ => None,
    };

    let target_hmon: HMONITOR = unsafe {
        match target_hwnd {
            Some(hwnd) => {
                use windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST;
                windows::Win32::Graphics::Gdi::MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)
            }
            None => match opts.source_id.as_deref() {
                Some(id) if id.starts_with("monitor:") => {
                    let v: usize = id[8..].parse().map_err(|e| anyhow::anyhow!("bad monitor: {}", e))?;
                    HMONITOR(v as *mut _)
                }
                _ => MonitorFromPoint(
                    windows::Win32::Foundation::POINT { x: 0, y: 0 },
                    MONITOR_DEFAULTTOPRIMARY,
                ),
            },
        }
    };

    // Create D3D11 device on correct adapter
    let matched_adapter = unsafe { find_adapter_for_monitor(target_hmon) };
    let (device, context, winrt_device) =
        unsafe { create_d3d11_device(matched_adapter.as_ref())? };

    // Create capture item
    let item = unsafe {
        match target_hwnd {
            Some(hwnd) => capture_item_from_window(hwnd)?,
            None => capture_item_from_monitor(target_hmon)?,
        }
    };

    let size = item.Size()?;
    let cap_w = size.Width as u32;
    let cap_h = size.Height as u32;
    let fps = opts.fps;

    // Create Video Processor (BGRA→NV12 + scaling)
    let vp_state = unsafe {
        VideoProcessorState::new(&device, cap_w, cap_h, OUTPUT_WIDTH, OUTPUT_HEIGHT)?
    };
    let vp_state = Arc::new(Mutex::new(vp_state));

    // Create NV12 pool pre-filled with legal black (AMD fix)
    let nv12_pool = unsafe {
        create_nv12_pool(&device, &context, OUTPUT_WIDTH, OUTPUT_HEIGHT, NV12_POOL_SIZE)?
    };

    // Create ONE persistent hardware H.264 encoder (constraint #1)
    let encoder = unsafe {
        PersistentEncoder::new(&device, OUTPUT_WIDTH, OUTPUT_HEIGHT, fps, opts.bitrate_kbps)?
    };
    let encoder = Arc::new(Mutex::new(encoder));

    // Create frame pool
    let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;

    let session = frame_pool.CreateCaptureSession(&item)?;
    session.SetIsCursorCaptureEnabled(true)?;
    let _ = session.SetIsBorderRequired(false);

    // Handle capture target closing
    {
        let stop_on_close = stop.clone();
        item.Closed(&TypedEventHandler::new(move |_, _| {
            stop_on_close.store(true, Ordering::SeqCst);
            Ok(())
        }))?;
    }

    // Send ready info
    let ready_info = CaptureReadyInfo {
        width: OUTPUT_WIDTH,
        height: OUTPUT_HEIGHT,
        fps,
        segment_dir: opts.segment_dir.to_string_lossy().to_string(),
    };
    let _ = ready_tx.send(Ok(ready_info));

    // Audio thread
    let base_time = Arc::new(AtomicI64::new(i64::MIN));
    let audio_thread = if !opts.no_audio {
        let ring_audio = ring.clone();
        let s = stop.clone();
        let bt = base_time.clone();
        let mic = opts.mic_device.clone();
        let lb = opts.loopback_device.clone();
        Some(thread::spawn(move || {
            gpu_audio_loop(s, mic, lb, ring_audio, bt);
        }))
    } else {
        None
    };

    // Track capture size for resize detection
    let cap_size = Arc::new((AtomicU32::new(cap_w), AtomicU32::new(cap_h)));

    // Frame counter for PTS calculation and NV12 pool rotation
    let frame_counter = Arc::new(AtomicUsize::new(0));
    let nv12_idx = Arc::new(AtomicUsize::new(0));

    // Set capture thread priority
    unsafe {
        use windows::Win32::System::Threading::*;
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
    }

    // Frame arrived callback
    let stop_cb = stop.clone();
    let context_cb = context.clone();
    let device_cb = device.clone();
    let ring_cb = ring.clone();
    let encoder_cb = encoder.clone();
    let vp_state_cb = vp_state.clone();
    let cap_size_cb = cap_size.clone();
    let frame_counter_cb = frame_counter.clone();
    let nv12_idx_cb = nv12_idx.clone();
    let base_time_cb = base_time.clone();

    struct SendDevice(IDirect3DDevice);
    unsafe impl Send for SendDevice {}
    unsafe impl Sync for SendDevice {}
    let winrt_device_cb = Arc::new(SendDevice(winrt_device.clone()));

    frame_pool.FrameArrived(&TypedEventHandler::new({
        move |pool: windows_core::Ref<Direct3D11CaptureFramePool>, _| {
            if stop_cb.load(Ordering::Relaxed) {
                return Ok(());
            }
            let pool_ref = pool.ok()?;
            let frame = match pool_ref.TryGetNextFrame() {
                Ok(f) => f,
                Err(_) => return Ok(()),
            };

            // Signal audio start on first frame
            if base_time_cb.load(Ordering::Acquire) == i64::MIN {
                base_time_cb.store(0, Ordering::Release);
            }

            // Get D3D11 texture from frame
            let surface = frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let frame_texture: ID3D11Texture2D = unsafe { access.GetInterface()? };

            // Detect resize
            if let Ok(content_size) = frame.ContentSize() {
                let (new_w, new_h) = (content_size.Width as u32, content_size.Height as u32);
                let (old_w, old_h) = (
                    cap_size_cb.0.load(Ordering::Relaxed),
                    cap_size_cb.1.load(Ordering::Relaxed),
                );
                if (new_w != old_w && new_w > 0) || (new_h != old_h && new_h > 0) {
                    // Recreate frame pool at new size
                    match pool_ref.Recreate(
                        &winrt_device_cb.0,
                        DirectXPixelFormat::B8G8R8A8UIntNormalized,
                        2,
                        content_size,
                    ) {
                        Ok(()) => {
                            cap_size_cb.0.store(new_w, Ordering::Relaxed);
                            cap_size_cb.1.store(new_h, Ordering::Relaxed);
                            // Update VP source dimensions
                            let mut vp = vp_state_cb.lock();
                            let _ = unsafe { vp.update_source_size(&device_cb, new_w, new_h) };
                        }
                        Err(e) => eprintln!("[gpu_capture] Recreate failed: {e}"),
                    }
                }
            }

            // Calculate PTS
            let frame_num = frame_counter_cb.fetch_add(1, Ordering::Relaxed) as i64;
            let pts_100ns = (frame_num * 10_000_000) / fps as i64;
            let next_pts = ((frame_num + 1) * 10_000_000) / fps as i64;
            let duration_100ns = next_pts - pts_100ns;

            // Pick NV12 pool texture
            let pool_idx = nv12_idx_cb.fetch_add(1, Ordering::Relaxed) % NV12_POOL_SIZE;
            let nv12_tex = &nv12_pool[pool_idx];

            // Video Processor: BGRA→NV12 + scale
            {
                let vp = vp_state_cb.lock();
                if let Err(e) = unsafe { vp.process(&frame_texture, nv12_tex) } {
                    eprintln!("[gpu_capture] VP process failed: {e}");
                    return Ok(());
                }
            }

            // Encode NV12→H.264
            {
                let enc = encoder_cb.lock();
                match unsafe { enc.encode_frame(nv12_tex, pts_100ns, duration_100ns, &device_cb, &context_cb, OUTPUT_WIDTH, OUTPUT_HEIGHT) } {
                    Ok(encoded_frames) => {
                        let mut ring = ring_cb.lock();
                        for ef in encoded_frames {
                            ring.push_video(ef);
                        }
                    }
                    Err(e) => {
                        eprintln!("[gpu_capture] encode failed: {e}");
                    }
                }
            }

            Ok(())
        }
    }))?;

    // Start capture
    session.StartCapture()?;

    // Wait for stop signal
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(std::time::Duration::from_millis(50));
    }

    // Stop capture
    session.Close()?;
    frame_pool.Close()?;

    // Wait for audio thread
    if let Some(t) = audio_thread {
        let _ = t.join();
    }

    // Flush encoder — get remaining frames
    {
        let enc = encoder.lock();
        if let Ok(remaining) = unsafe { enc.flush() } {
            let mut ring_lock = ring.lock();
            for ef in remaining {
                ring_lock.push_video(ef);
            }
        }
    }

    unsafe {
        MFShutdown()?;
    }
    Ok(())
}



// ── Audio Capture Loop ────────────────────────────────────────────────────────

/// Audio capture loop: captures 48kHz stereo PCM and pushes to the ring buffer.
/// AAC encoding happens only at mux time (constraint #10).
fn gpu_audio_loop(
    stop: Arc<AtomicBool>,
    mic_device: Option<String>,
    loopback: Option<String>,
    ring: Arc<Mutex<EncodedMediaRing>>,
    base_time: Arc<AtomicI64>,
) {
    unsafe {
        use windows::Win32::System::Threading::*;
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }

    // Wait for first video frame
    while base_time.load(Ordering::Acquire) == i64::MIN {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(1));
    }

    let audio_sample_counter = Arc::new(AtomicUsize::new(0));
    let ring_clone = ring.clone();
    let counter_clone = audio_sample_counter.clone();

    let res = WasapiCapture::capture_to_callback(stop, mic_device, loopback, move |chunk: &[f32]| {
        let n_frames = chunk.len() / AUDIO_CHANNELS as usize;
        let sample_offset = counter_clone.fetch_add(n_frames, Ordering::Relaxed);
        let pts_100ns = (sample_offset as i64 * 10_000_000) / AUDIO_SAMPLE_RATE as i64;
        let duration_100ns = (n_frames as i64 * 10_000_000) / AUDIO_SAMPLE_RATE as i64;

        let audio_chunk = AudioChunk {
            data: chunk.to_vec(),
            pts_100ns,
            duration_100ns,
        };
        ring_clone.lock().push_audio(audio_chunk);
    });

    if let Err(e) = res {
        eprintln!("[gpu_audio] error: {e}");
    }
}

// ── Source Listing ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    pub id: String,
    pub name: String,
    pub source_type: String,
    pub width: i32,
    pub height: i32,
}

pub fn list_sources() -> Vec<SourceInfo> {
    let mut sources: Vec<SourceInfo> = Vec::new();
    unsafe {
        use windows::Win32::Foundation::{LPARAM, RECT};
        use windows::Win32::Graphics::Gdi::*;
        use windows_core::BOOL;

        extern "system" fn mon_cb(
            hmon: HMONITOR,
            _: HDC,
            _: *mut RECT,
            lp: LPARAM,
        ) -> BOOL {
            let list = unsafe { &mut *(lp.0 as *mut Vec<SourceInfo>) };
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if unsafe { GetMonitorInfoW(hmon, &mut info.monitorInfo).as_bool() } {
                let name = String::from_utf16_lossy(
                    &info.szDevice.iter().take_while(|&&c| c != 0).cloned().collect::<Vec<_>>(),
                );
                let r = &info.monitorInfo.rcMonitor;
                list.push(SourceInfo {
                    id: format!("monitor:{}", hmon.0 as usize),
                    name: format!("Display {}", name.trim()),
                    source_type: "monitor".into(),
                    width: r.right - r.left,
                    height: r.bottom - r.top,
                });
            }
            BOOL(1)
        }

        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(mon_cb),
            LPARAM(&mut sources as *mut _ as isize),
        );

        use windows::Win32::UI::WindowsAndMessaging::*;

        extern "system" fn win_cb(hwnd: HWND, lp: LPARAM) -> BOOL {
            let list = unsafe { &mut *(lp.0 as *mut Vec<SourceInfo>) };
            if !unsafe { IsWindowVisible(hwnd).as_bool() } {
                return BOOL(1);
            }
            let mut t = [0u16; 512];
            let len = unsafe { GetWindowTextW(hwnd, &mut t) };
            if len == 0 {
                return BOOL(1);
            }
            let title = String::from_utf16_lossy(&t[..len as usize]);
            let mut r = windows::Win32::Foundation::RECT::default();
            let _ = unsafe { GetWindowRect(hwnd, &mut r) };
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            if w < 150 || h < 150 {
                return BOOL(1);
            }
            list.push(SourceInfo {
                id: format!("hwnd:{}", hwnd.0 as usize),
                name: title,
                source_type: "window".into(),
                width: w,
                height: h,
            });
            BOOL(1)
        }

        let _ = EnumWindows(Some(win_cb), LPARAM(&mut sources as *mut _ as isize));
    }
    sources
}
