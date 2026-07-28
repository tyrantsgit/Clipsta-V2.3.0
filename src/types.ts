export interface AppSettings {
	outputFolder: string;
	hotkeyClip30Sec: string;
	hotkeyClip1Min: string;
	hotkeyClip5Min: string;
	hotkeyRecord: string;
	bufferDuration: number;
	resolution: string;
	fps: number;
	aspectRatio: string;
	encoder: string;
	bitrate: number;
	audioBitrate: number;
	captureAudio: boolean;
	captureMic: boolean;
	audioSource: string;
	audioInputDeviceId: string;
	gameDetect: boolean;
	autoUpload: boolean;
	minimizeToTray: boolean;
	overlayEnabled: boolean;
	clipSoundEnabled: boolean;
	cloudEnabled: boolean;
	cloudPairCode: string;
	uploadBandwidth: number;
	deleteAfterUpload: boolean;
	desktopDeviceId: string;
	desktopAudioDeviceId: string;
	orientation: string;
	watchFolderPath: string;
	watchFolderEnabled: boolean;
}

export interface UploadJob {
	id: string;
	path: string;
	name: string;
	size: number;
	progress: number;
	status: "queued" | "uploading" | "done" | "failed";
	error?: string;
	streamUid?: string;
	shareUrl?: string;
	trimStart?: number;
	trimEnd?: number;
	cuts?: { start: number; end: number }[];
	retryCount?: number;
}

export interface UploadClipResponse {
	id: string;
	streamUid: string;
	uploadUrl: string;
	shareUrl: string;
	playerUrl: string;
	streamUrl: string;
	thumbnailUrl: string;
}

export interface ScreenSource {
	id: string;
	name: string;
	thumbnail: string;
	appIcon: string | null;
}

export interface WgcSource {
	id: string;
	name: string;
	source_type: "monitor" | "window";
	width: number;
	height: number;
}

export interface AudioDevice {
	id: string;
	kind: "input" | "output";
	name: string;
}

export interface ClipFile {
	name: string;
	path: string;
	size: number;
	createdAt: string;
}

export type Page = "capture" | "library" | "editor" | "settings";

export interface UploadClipOpts {
	desktopDeviceId: string;
	filePath: string;
	fileName: string;
	durationSeconds: number;
	bytes: number;
	capturedAt: string;
	encoder?: string;
	trimStart?: number;
	trimEnd?: number;
	cuts?: { start: number; end: number }[];
}

export interface UploadStatusBody {
	desktopDeviceId: string;
	desktopName: string;
	queuedCount: number;
	waitingForGameplayCount: number;
	uploadingCount: number;
	uploadedCount: number;
	failedCount: number;
	currentProgressPercent: number;
	currentStatus: string;
}

export interface FileStats {
	size: number;
	modifiedAt: string;
}

export interface TimelineEntry {
	id: string;
	path: string;
	name: string;
	trimIn: number;
	trimOut: number;
}

export interface ExportOpts {
	format: string;
	aspectRatio: string;
	resolution: string;
	fps?: number;
	encoder?: string;
	trimStart?: number;
	trimEnd?: number;
	cuts?: { start: number; end: number }[];
	timeline?: { path: string; trimIn: number; trimOut: number }[];
}

export interface CloudConfig {
	apiBase: string;
	apiKey: string;
}

export interface SystemInfo {
	platform: string;
	arch: string;
	totalMem: number;
	freeMem: number;
	cpus: number;
}

export interface WgcStartOpts {
	sourceId: string | null;
	fps: number;
	noAudio: boolean;
	micDevice?: string;
	loopbackDevice?: string;
}

export interface WgcStartResult {
	width: number;
	height: number;
	fps: number;
}

export interface WgcSaveClipOpts {
	seconds: number;
	fileName: string;
	sourceId: string | null;
	fps?: number;
	noAudio?: boolean;
	micDevice?: string;
	loopbackDevice?: string;
}


export interface Mp4Info {
	duration: number;
	fps: number;
	width: number;
	height: number;
	videoCodec: string;
	audioCodec: string;
	bitrate: number;
	hasAudio: boolean;
	keyframes: number[];
}

export interface TrimResult {
	outputPath: string;
	requestedStart: number;
	actualStart: number;
	requestedEnd: number;
	actualEnd: number;
	duration: number;
	extraBefore: number;
}

