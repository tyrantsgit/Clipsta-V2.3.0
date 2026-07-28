import { useCallback, useEffect, useRef, useState } from "react";
import type { AppSettings } from "../types";
import bridge from "../tauri-bridge";

export const DEFAULTS: AppSettings = {
	outputFolder: "",
	hotkeyClip30Sec: "Ctrl+Shift+G",
	hotkeyClip1Min: "Alt+F9",
	hotkeyClip5Min: "Alt+F10",
	hotkeyRecord: "F9",
	bufferDuration: 300,
	resolution: "1080p",
	fps: 60,
	aspectRatio: "16:9",
	encoder: "auto",
	bitrate: 20000,
	audioBitrate: 192,
	captureAudio: true,
	captureMic: false,
	audioSource: "both",
	audioInputDeviceId: "",
	gameDetect: true,
	autoUpload: false,
	minimizeToTray: true,
	overlayEnabled: true,
	clipSoundEnabled: true,
	cloudEnabled: false,
	cloudPairCode: "",
	uploadBandwidth: 0,
	deleteAfterUpload: false,
	desktopDeviceId: "",
	desktopAudioDeviceId: "",
	orientation: "landscape",
	watchFolderPath: "",
	watchFolderEnabled: false,
};

export function useSettings() {
	const [settings, setSettings] = useState<AppSettings>(DEFAULTS);
	const [loaded, setLoaded] = useState(false);
	const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);

	useEffect(() => {
		(async () => {
			try {
				const s = await bridge.getSettings();
				setSettings((prev) => ({ ...prev, ...s }));
			} catch {
				// Use defaults if backend not ready yet
			}
			setLoaded(true);
		})();
	}, []);

	const updateSetting = useCallback(<K extends keyof AppSettings>(key: K, value: AppSettings[K]) => {
		setSettings((prev) => ({ ...prev, [key]: value }));
		const isHotkey = String(key).startsWith("hotkey");
		if (isHotkey) {
			bridge.setSetting(key, value).catch(() => {});
		} else {
			if (debounceRef.current) clearTimeout(debounceRef.current);
			debounceRef.current = setTimeout(() => {
				bridge.setSetting(key, value).catch(() => {});
			}, 500);
		}
	}, []);

	const saveAll = useCallback(async (s: AppSettings) => {
		setSettings(s);
		await bridge.setAllSettings(s);
		// Re-register hotkeys on backend (setAllSettings already does this,
		// but resumeHotkeys is a safety net if the frontend suspended them)
		await bridge.resumeHotkeys().catch(() => {});
	}, []);

	return { settings, loaded, updateSetting, saveAll };
}
