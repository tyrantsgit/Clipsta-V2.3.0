/**
 * Tauri Bridge — replaces Electron's preload/contextBridge.
 * Maps every window.clipsta.* method to Tauri invoke() or event listen().
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open, save } from "@tauri-apps/plugin-dialog";
import { openPath } from "@tauri-apps/plugin-opener";
import type {
	AppSettings,
	AudioDevice,
	ClipFile,
	CloudConfig,
	ExportOpts,
	FileStats,
	Mp4Info,
	ScreenSource,
	SystemInfo,
	TrimResult,
	UploadClipOpts,
	UploadClipResponse,
	UploadStatusBody,
	WgcSaveClipOpts,
	WgcSource,
	WgcStartOpts,
	WgcStartResult,
} from "./types";

// ── Window Controls ─────────────────────────────────────────────────────────
export function minimize() {
	getCurrentWindow().minimize();
}

export function maximize() {
	getCurrentWindow().toggleMaximize();
}

export function close() {
	getCurrentWindow().close();
}

// ── Sources ─────────────────────────────────────────────────────────────────
export function getSources(): Promise<ScreenSource[]> {
	return invoke<ScreenSource[]>("wgc_sources");
}

export function getWgcSources(): Promise<WgcSource[]> {
	return invoke<WgcSource[]>("wgc_sources");
}

// ── Recording ───────────────────────────────────────────────────────────────
export function setPendingSource(_id: string | null): Promise<void> {
	// No-op in Tauri — not needed (WGC source set via start_recording)
	return Promise.resolve();
}

export function setRecordingState(_recording: boolean): Promise<void> {
	// No-op in Tauri — recording state is managed by the Rust CaptureSession internally
	return Promise.resolve();
}

export function saveRecording(_buf: ArrayBuffer, _name: string): Promise<string> {
	// No-op in Tauri — recording is saved via wgc_save_clip
	return Promise.resolve("");
}

export function exportRecording(inputPath: string, outputPath: string, opts: ExportOpts): Promise<string> {
	return invoke<string>("recording_export", { inputPath, outputPath, opts });
}

export async function browseSaveExport(defaultName: string): Promise<string | null> {
	const result = await save({
		defaultPath: defaultName,
		filters: [
			{ name: "Video", extensions: ["mp4", "webm", "mkv", "mov"] },
		],
	});
	return result ?? null;
}

// ── WGC Recording ───────────────────────────────────────────────────────────
export function wgcStartRecording(opts: WgcStartOpts): Promise<WgcStartResult | null> {
	return invoke<WgcStartResult | null>("wgc_start_recording", { opts });
}

export function wgcSaveClip(opts: WgcSaveClipOpts): Promise<string | null> {
	return invoke<string | null>("wgc_save_clip", { seconds: opts.seconds, fileName: opts.fileName });
}

export function wgcStopRecording(): Promise<void> {
	return invoke("wgc_stop_recording");
}

export function wgcPauseToggle(): Promise<void> {
	// Not implemented in Tauri version
	return Promise.resolve();
}

export function wgcSaveFullRecording(): Promise<string | null> {
	return invoke<string | null>("wgc_save_full_recording");
}

export function onWgcClipSaved(cb: (path: string) => void): UnlistenFn | Promise<UnlistenFn> {
	return listen<string>("wgc:clipSaved", (event) => cb(event.payload));
}

export function onWgcLog(cb: (level: string, line: string) => void): UnlistenFn | Promise<UnlistenFn> {
	return listen<{ level: string; line: string }>("wgc-log", (event) => {
		cb(event.payload.level, event.payload.line);
	});
}

// ── Hotkey Events ───────────────────────────────────────────────────────────
// Rust emits "hotkey:clip" with seconds (30/60/300) as payload
export function onHotkeyRecord(cb: () => void): Promise<UnlistenFn> {
	return listen("hotkey:record", () => cb());
}

export function onHotkeyClip30Sec(cb: () => void): Promise<UnlistenFn> {
	return listen<number>("hotkey:clip", (event) => {
		if (event.payload === 30) cb();
	});
}

export function onHotkeyClip1Min(cb: () => void): Promise<UnlistenFn> {
	return listen<number>("hotkey:clip", (event) => {
		if (event.payload === 60) cb();
	});
}

export function onHotkeyClip5Min(cb: () => void): Promise<UnlistenFn> {
	return listen<number>("hotkey:clip", (event) => {
		if (event.payload === 300) cb();
	});
}

export function onPlayClipSound(cb: () => void): Promise<UnlistenFn> {
	return listen("play-clip-sound", () => cb());
}

// ── Settings ────────────────────────────────────────────────────────────────
export function getSettings(): Promise<AppSettings> {
	return invoke<AppSettings>("settings_get_all");
}

export function setSetting(key: string, value: unknown): Promise<boolean> {
	return invoke<boolean>("settings_set", { key, value });
}

export function setAllSettings(s: Partial<AppSettings>): Promise<boolean> {
	return invoke<boolean>("settings_set_all", { settings: s });
}

export function suspendHotkeys(): Promise<boolean> {
	return invoke<boolean>("hotkeys_suspend");
}

export function resumeHotkeys(): Promise<boolean> {
	return invoke<boolean>("hotkeys_resume");
}

// ── Dialogs ─────────────────────────────────────────────────────────────────
export async function browseFolder(): Promise<string | null> {
	const result = await open({ directory: true, multiple: false });
	return result ?? null;
}

export async function browseImportFolder(): Promise<string | null> {
	const result = await open({ directory: true, multiple: false, title: "Select folder to import" });
	return result ?? null;
}

export async function browseFile(): Promise<string | null> {
	const result = await open({
		multiple: false,
		filters: [{ name: "Video", extensions: ["mp4", "webm", "mkv", "mov"] }],
	});
	return result ?? null;
}

// ── Shell ───────────────────────────────────────────────────────────────────
export async function openFolder(p: string): Promise<void> {
	await openPath(p);
}

export async function openFile(p: string): Promise<void> {
	await openPath(p);
}

export function showInFolder(p: string): Promise<void> {
	return invoke("shell_show_item", { path: p });
}

// ── Audio Devices ───────────────────────────────────────────────────────────
export function listAudioDevices(): Promise<AudioDevice[]> {
	return invoke<AudioDevice[]>("audio_list_devices");
}

export function getDefaultAudioDevices(): Promise<{ defaultOutputId: string; defaultInputId: string }> {
	return invoke<{ defaultOutputId: string; defaultInputId: string }>("audio_default_devices");
}

// ── Clips ───────────────────────────────────────────────────────────────────
export function listClips(): Promise<ClipFile[]> {
	return invoke<ClipFile[]>("clips_list");
}

export function deleteClip(path: string): Promise<boolean> {
	return invoke<boolean>("clips_delete", { path });
}

export function renameClip(oldPath: string, newName: string): Promise<string> {
	return invoke<string>("clips_rename", { oldPath, newName });
}

export function importClip(sourcePath: string): Promise<string> {
	return invoke<string>("clips_import", { sourcePath });
}

export function importFolder(folderPath: string): Promise<string[]> {
	return invoke<string[]>("clips_import_folder", { folderPath });
}

// ── System ──────────────────────────────────────────────────────────────────
export function getSystemInfo(): Promise<SystemInfo> {
	return invoke<SystemInfo>("system_info");
}

/** Get the title of the active foreground window (for ShadowPlay-style game detection) */
export function getActiveWindowTitle(): Promise<string> {
	return invoke<string>("get_active_window_title");
}

// ── File Operations ─────────────────────────────────────────────────────────
export function readFile(_path: string): Promise<ArrayBuffer> {
	// Not directly needed — use Tauri fs plugin for file reads
	return Promise.resolve(new ArrayBuffer(0));
}

export function getFileStats(path: string): Promise<FileStats> {
	return invoke<FileStats>("file_stat", { filePath: path });
}

export function ensureDir(path: string): Promise<boolean> {
	return invoke<boolean>("file_ensure_dir", { dirPath: path });
}

export function copyToDownloads(filePath: string): Promise<string> {
	return invoke<string>("file_copy_to_downloads", { filePath });
}

// ── Cloud ───────────────────────────────────────────────────────────────────
const CLOUD_CONFIG: CloudConfig = {
	apiBase: "https://clipsta-api.godson594.workers.dev",
	apiKey: "32b28eac803a1b24c19e20665919eaeb7f1493d2b5e3f68be7944db6d9f01b96",
};

export function getCloudConfig(): Promise<CloudConfig> {
	return Promise.resolve(CLOUD_CONFIG);
}

export async function uploadClip(opts: UploadClipOpts): Promise<UploadClipResponse> {
	const cfg = CLOUD_CONFIG;

	// Compress large clips before upload: re-encode to 720p at lower bitrate.
	// This reduces a 300MB 1440p clip to ~30-50MB 720p — fast to upload and view.
	let uploadFilePath = opts.filePath;
	let uploadBytes = opts.bytes;
	let uploadFileName = opts.fileName;
	let tempCompressed: string | null = null;

	if (opts.bytes > 50 * 1024 * 1024) { // Only compress if >50MB
		try {
			// Use Rust-side Media Foundation compression for reliability
			const compressedPath: string | null = await invoke("compress_for_upload", { filePath: opts.filePath });
			if (compressedPath) {
				const { stat } = await import("@tauri-apps/plugin-fs");
				const compStat = await stat(compressedPath);
				if (compStat.size && compStat.size > 0) {
					uploadFilePath = compressedPath;
					uploadBytes = compStat.size;
					uploadFileName = compressedPath.replace(/^.*[\\/]/, "");
					tempCompressed = compressedPath;
				}
			}
		} catch (e) {
			// Compression failed — fall back to original file
			console.warn("[upload] compression failed, using original:", e);
		}
	}

	// Reject files still over 200MB after compression
	if (uploadBytes > 200 * 1024 * 1024) {
		if (tempCompressed) { try { const { remove } = await import("@tauri-apps/plugin-fs"); await remove(tempCompressed); } catch {} }
		throw new Error(`File too large for upload (${Math.round(uploadBytes / 1024 / 1024)}MB). Max 200MB.`);
	}

	// Step 1: Request upload URL from API
	const clipRes = await fetch(`${cfg.apiBase}/clip-uploads`, {
		method: "POST",
		headers: {
			"Content-Type": "application/json",
			"X-Clipsta-Test-Key": cfg.apiKey,
		},
		body: JSON.stringify({
			desktopDeviceId: opts.desktopDeviceId,
			fileName: uploadFileName,
			durationSeconds: opts.durationSeconds,
			bytes: uploadBytes,
			capturedAt: opts.capturedAt,
		}),
	});
	if (!clipRes.ok) {
		if (tempCompressed) { try { const { remove } = await import("@tauri-apps/plugin-fs"); await remove(tempCompressed); } catch {} }
		const errBody = await clipRes.text().catch(() => "");
		throw new Error(`clip-uploads failed: HTTP ${clipRes.status} ${errBody}`);
	}
	const clipData = await clipRes.json() as UploadClipResponse & { uploadUrl: string };

	// Step 2: Read compressed file and upload
	const { readFile } = await import("@tauri-apps/plugin-fs");
	let fileBytes: Uint8Array;
	try {
		fileBytes = await readFile(uploadFilePath);
	} catch (e: any) {
		if (tempCompressed) { try { const { remove } = await import("@tauri-apps/plugin-fs"); await remove(tempCompressed); } catch {} }
		throw new Error(`Failed to read clip file: ${e?.message ?? e}`);
	}

	const mime = opts.fileName.endsWith(".webm") ? "video/webm"
		: opts.fileName.endsWith(".mkv") ? "video/x-matroska"
		: opts.fileName.endsWith(".mov") ? "video/quicktime"
		: "video/mp4";
	const blob = new Blob([fileBytes], { type: mime });
	const formData = new FormData();
	formData.append("file", blob, opts.fileName);

	const uploadRes = await fetch(clipData.uploadUrl, { method: "POST", body: formData });
	if (!uploadRes.ok) {
		if (tempCompressed) { try { const { remove } = await import("@tauri-apps/plugin-fs"); await remove(tempCompressed); } catch {} }
		throw new Error(`Upload failed: HTTP ${uploadRes.status}`);
	}

	// Clean up temp compressed file
	if (tempCompressed) { try { const { remove } = await import("@tauri-apps/plugin-fs"); await remove(tempCompressed); } catch {} }

	return clipData;
}

export async function notifyUploadStatus(body: UploadStatusBody): Promise<void> {
	const cfg = CLOUD_CONFIG;
	await fetch(`${cfg.apiBase}/desktop-upload-status`, {
		method: "POST",
		headers: {
			"Content-Type": "application/json",
			"X-Clipsta-Test-Key": cfg.apiKey,
		},
		body: JSON.stringify(body),
	}).catch(() => {});
}

// ── MP4 Inspection ──────────────────────────────────────────────────────────
export function inspectMp4(filePath: string): Promise<Mp4Info> {
	return invoke<Mp4Info>('mp4_inspect', { filePath });
}

export function getMp4Keyframes(filePath: string): Promise<number[]> {
	return invoke<number[]>('mp4_keyframes', { filePath });
}

// ── Lossless Trim ───────────────────────────────────────────────────────────
export function losslessTrimClip(
	inputPath: string,
	outputPath: string,
	start: number,
	end: number,
): Promise<TrimResult> {
	return invoke<TrimResult>('lossless_trim_clip', { inputPath, outputPath, start, end });
}

// ── Watch Folder ────────────────────────────────────────────────────────────
export function watchFolderStart(): Promise<boolean> {
	return invoke<boolean>("watch_folder_start");
}

export function watchFolderStop(): Promise<boolean> {
	return invoke<boolean>("watch_folder_stop");
}

export function watchFolderStatus(): Promise<{ active: boolean; filesDetected: number }> {
	return invoke<{ active: boolean; filesDetected: number }>("watch_folder_status");
}

export function onWatchFolderNewFile(
	callback: (file: { path: string; name: string; size: number }) => void,
): Promise<UnlistenFn> {
	return listen<{ path: string; name: string; size: number }>("watch-folder:new-file", (event) => {
		callback(event.payload);
	});
}

// ── Convenience: unified bridge object ──────────────────────────────────────
const bridge = {
	minimize,
	maximize,
	close,
	getSources,
	getWgcSources,
	setPendingSource,
	setRecordingState,
	saveRecording,
	exportRecording,
	browseSaveExport,
	wgcStartRecording,
	wgcSaveClip,
	wgcStopRecording,
	wgcPauseToggle,
	wgcSaveFullRecording,
	onWgcClipSaved,
	onWgcLog,
	onHotkeyRecord,
	onHotkeyClip30Sec,
	onHotkeyClip1Min,
	onHotkeyClip5Min,
	onPlayClipSound,
	getSettings,
	setSetting,
	setAllSettings,
	suspendHotkeys,
	resumeHotkeys,
	browseFolder,
	browseImportFolder,
	browseFile,
	openFolder,
	openFile,
	showInFolder,
	listAudioDevices,
	getDefaultAudioDevices,
	listClips,
	deleteClip,
	renameClip,
	importClip,
	importFolder,
	getSystemInfo,
	getActiveWindowTitle,
	readFile,
	getFileStats,
	ensureDir,
	copyToDownloads,
	getCloudConfig,
	uploadClip,
	notifyUploadStatus,
	inspectMp4,
	getMp4Keyframes,
	losslessTrimClip,
	watchFolderStart,
	watchFolderStop,
	watchFolderStatus,
	onWatchFolderNewFile,
};

export default bridge;
