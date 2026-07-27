# Clipsta v1.9.0 — Desktop Game Clip Recorder

A high-performance gaming clip recorder for Windows, built with Tauri 2 + React + Rust. Records continuously in the background using Windows Graphics Capture (WGC) and lets you save the last 30s / 1min / 5min with a hotkey.

## Features

- **Always-on recording** — WGC capture runs in the background with minimal CPU/GPU impact
- **Instant clip saves** — Global hotkeys save the last N seconds without interrupting gameplay
- **Hardware encoding** — NVENC (NVIDIA), AMF (AMD), QuickSync (Intel) via Media Foundation
- **NV12 pipeline** — 62% less bandwidth than BGRA with direct GPU encode path
- **Segmented buffer** — Rolling MP4 segments with configurable buffer duration (up to 5 min)
- **Built-in editor** — Trim, cut, merge clips with timeline UI
- **Cloud upload** — Pair with iPhone app, auto-upload clips
- **System tray** — Minimize to tray, clip from tray menu
- **Audio capture** — WASAPI loopback (desktop audio) + mic input with auto-detection

## Architecture

```
Frontend (React + TypeScript)
  └── tauri-bridge.ts (IPC layer)
        └── Tauri 2 invoke/events
              └── Rust Backend
                    ├── capture.rs  — WGC + MF SinkWriter segmented recording
                    ├── audio.rs    — WASAPI loopback + mic capture
                    ├── commands.rs — All Tauri IPC command handlers
                    ├── settings.rs — JSON settings store
                    └── lib.rs      — App setup, tray, global shortcuts
```

## Prerequisites

- **Windows 10/11** (WGC requires Windows 10 1903+)
- **Node.js 18+**
- **Rust 1.70+** (install via [rustup](https://rustup.rs/))
- **FFmpeg** — Place `ffmpeg.exe` in `src-tauri/resources/` (not included in repo due to 100MB GitHub limit)

### Getting FFmpeg

Download a static build from https://www.gyan.dev/ffmpeg/builds/ (get `ffmpeg-release-essentials.zip`), extract `ffmpeg.exe`, and place it at:
```
src-tauri/resources/ffmpeg.exe
```

## Development

```bash
npm install
npm run tauri dev
```

## Production Build

```bash
npm run tauri build
```

Output: `src-tauri/target/release/bundle/nsis/Clipsta_1.9.0_x64-setup.exe`

The installer is fully self-contained (includes ffmpeg.exe, WebView2 bootstrapper, all assets).

## Project Structure

```
clipsta-tauri/
├── src/                          # React frontend
│   ├── App.tsx                   # Main app shell + routing
│   ├── tauri-bridge.ts           # Tauri IPC bridge (replaces Electron preload)
│   ├── types.ts                  # Shared TypeScript types
│   ├── utils.ts                  # Shared utilities
│   ├── hooks/
│   │   ├── useRecorder.ts        # Recording state management
│   │   ├── useSettings.ts        # Settings state + persistence
│   │   └── useCloudUpload.ts     # Cloud upload queue management
│   └── components/pages/
│       ├── LibraryPage.tsx        # Clip library + video player
│       ├── EditorPage.tsx         # Timeline editor with trim/cut/merge
│       └── SettingsPage.tsx       # Settings UI with hotkey capture
├── src-tauri/                    # Rust backend
│   ├── src/
│   │   ├── lib.rs                # Tauri app setup, tray, hotkeys
│   │   ├── capture.rs            # WGC + Media Foundation segmented capture
│   │   ├── audio.rs              # WASAPI audio capture
│   │   ├── commands.rs           # All IPC command handlers
│   │   └── settings.rs           # JSON settings store
│   ├── resources/
│   │   └── ffmpeg.exe            # (not in repo — see Prerequisites)
│   ├── Cargo.toml
│   └── tauri.conf.json
├── package.json
└── vite.config.ts
```

## Key Design Decisions

1. **Segmented recording** — 10-second MP4 segments allow instant clip saves without stopping the recording. Old segments are pruned when they exceed the buffer duration.

2. **NV12 input format** — BGRA→NV12 conversion before encoding reduces memory bandwidth by 62% and is the native input format for hardware encoders.

3. **Stream copy for clips** — FFmpeg concat demuxer with `-c:v copy` produces clips instantly without re-encoding. Timestamp flags (`-fflags +genpts+discardcorrupt+igndts`, `-segment_time_metadata 1`, `-enc_time_base -1`) prevent frame skips at segment boundaries.

4. **Asset protocol for video playback** — Tauri 2's WebView2 blocks `file://` URLs. All video playback uses `convertFileSrc()` which routes through `https://asset.localhost/` protocol.

5. **Global shortcuts re-registration** — Hotkeys are re-registered on the native side whenever settings are saved, ensuring changed hotkeys take effect immediately.

## Changelog (v1.9.0)

- Fixed: Hotkeys not working after changing them in settings
- Fixed: Clip creation failing (hotkey → save clip pipeline)
- Fixed: In-app media player not playing videos (file:// → asset:// protocol)
- Fixed: Frame skips in clip playback (FFmpeg concat timestamp handling)
- Fixed: Drag-drop into editor causing "failed to play video" (Tauri 2 native drag-drop)
- Fixed: Cloud pairing resetting auto-upload settings
- Added: Auto-detection of system default audio devices on first launch
- Removed: Redundant sidebar clip save buttons (Status page already has them)
- Added: `protocol-asset` feature for secure local file access in WebView2

## License

Proprietary — Clipsta © 2026
