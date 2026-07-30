# Clipsta v2.3 — Instant Replay for PC

ShadowPlay-style instant replay built with Tauri v2, Windows Graphics Capture, and Media Foundation hardware encoding.

## Architecture

```
Windows Graphics Capture (WGC)
→ Direct3D 11 BGRA texture (native screen resolution)
→ ID3D11VideoProcessor (scale + BGRA-to-NV12 color conversion)
→ One persistent asynchronous H.264 Media Foundation encoder (NVENC/AMD VCE)
→ Dedicated encoder thread (blocking GetEvent loop)
→ EncodedMediaRing (in-memory H.264 frames + PCM audio, 5-minute rolling buffer)
→ Keyframe-aligned slice on save (NAL unit type 5/7 + MFSampleExtension_CleanPoint)
→ Media Foundation Sink Writer (H.264 passthrough + PCM→AAC encoding)
→ H.264/AAC MP4 output
```

## Output Format (matches ShadowPlay)

| Property | Value |
|----------|-------|
| Resolution | 1920×1088 (16-pixel aligned) |
| Aspect Ratio | 16:9 |
| Codec | H.264 High Profile, Level 4.2 |
| FPS | 60 fps (wall-clock PTS) |
| Rate Control | CBR ~8 Mbps |
| Audio | AAC-LC, 48kHz, Stereo |
| Container | MP4 (mp42/isom) |
| File naming | `{GameName} {YYYY.MM.DD} - {HH.MM.SS.ff}.DVR.mp4` |

## Key Features

- **Instant Replay** — Continuously buffers last 5 minutes (configurable), save clips instantly via hotkey
- **Zero re-encoding on save** — H.264 passthrough muxing (~67ms to save a 60s clip)
- **Minimal FPS impact** — Dedicated encoder thread, non-blocking WGC callback, CBR at 1080p
- **Game detection** — Active window title polled for ShadowPlay-style clip naming
- **Desktop + game audio** — WASAPI loopback capture, always-on (per spec)
- **Optional microphone** — Mixed into audio stream when enabled
- **Cloud upload** — Paired mobile device upload with retry/backoff
- **Library** — Auto-refreshes on clip save, recursive folder scan, in-app preview
- **Global hotkeys** — Ctrl+Shift+G (30s), Alt+F9 (1min), Alt+F10 (5min)
- **System tray** — Minimize to tray, context menu for quick saves
- **NSIS installer** — Kills running processes before install, currentUser mode

## Technical Guardrails

| # | Constraint | Implementation |
|---|-----------|----------------|
| 1 | One live hardware encoder per session | ✅ Single PersistentEncoder for entire session, never recreated |
| 2 | 16-pixel aligned dimensions | ✅ 1920×1088 (both ÷16 cleanly) |
| 3 | Pin VP source/dest rectangles | ✅ VideoProcessorSetStreamSourceRect + DestRect + OutputTargetRect (NVIDIA fix) |
| 4 | Pre-fill NV12 pool with legal black | ✅ Y=16, U=V=128 via staging texture (AMD green-line fix) |
| 5 | Set rate control after SetOutputType | ✅ NVIDIA requires SetOutputType → D3D Manager → ICodecAPI order |
| 6 | Configure both bitrate and VBV buffer | ✅ CBR mode + VBV = 2× bitrate via ICodecAPI |
| 7 | Detect keyframes by NAL unit parsing | ✅ NAL type 5/7 + MFSampleExtension_CleanPoint fallback |
| 8 | Clean frame/audio grid timestamps | ✅ Wall-clock PTS for video + sample counter PTS for audio (both from session start) |
| 9 | Desktop audio always on | ✅ WASAPI loopback mandatory, only mic is optional |
| 10 | Don't block save guard while uploading | ✅ is_saving released immediately after mux, uploads queued separately |
| 11 | No FFmpeg/OBS bundled for core path | ✅ Core pipeline is pure MF/D3D11 |

## File Structure

| File | Responsibility |
|------|---------------|
| `src-tauri/src/gpu_capture.rs` | WGC capture, D3D11 VideoProcessor, persistent encoder, ring buffer, save/mux |
| `src-tauri/src/commands.rs` | Tauri IPC handlers (start/stop/save recording, clips, settings, export) |
| `src-tauri/src/settings.rs` | Settings store (JSON persistence, RwLock) |
| `src-tauri/src/audio.rs` | WASAPI desktop loopback + mic capture |
| `src-tauri/src/lib.rs` | Tauri app setup, tray, global shortcuts |
| `src/hooks/useRecorder.ts` | Frontend capture state, auto-start, hotkey handling |
| `src/hooks/useCloudUpload.ts` | Cloud pairing, upload queue, retry logic |
| `src/components/pages/LibraryPage.tsx` | Clip library with auto-refresh |
| `src/components/pages/SettingsPage.tsx` | Settings UI |

## Build

```bash
# Requirements: Rust, Node.js, Windows 10/11 SDK
npm install
npx tauri build
# Output: src-tauri/target/release/bundle/nsis/Clipsta_2.3.0_x64-setup.exe
```

## Fixes Applied (v2.3)

- **Audio desync fixed**: Video PTS uses wall-clock elapsed time from session start (matching audio's sample-counter wall-clock basis). Previously used frame counter which drifted from real time.
- **Capture fail fixed**: Removed D3D device manager before SetOutputType (caused `0xC00D6D76`). Correct order: SetOutputType → SET_D3D_MANAGER → ICodecAPI.
- **Keyframe detection fixed**: Added `MFSampleExtension_CleanPoint` fallback + `CODECAPI_AVEncMPVGOPSize` (2s GOP). Hardware encoder uses MFT-provided output samples (`MFT_OUTPUT_STREAM_PROVIDES_SAMPLES`).
- **Mux error fixed**: Added `MF_MT_AVG_BITRATE` to Sink Writer media types (fixes `0xC00D36B4`).
- **Library scan**: Now recursive — finds clips in game-name subfolders (ShadowPlay style).
- **Default folder**: `C:\Users\{user}\Videos\Clipsta`
