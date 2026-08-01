import { useCallback, useEffect, useRef, useState } from "react";
import type { AppSettings } from "../types";
import bridge from "../tauri-bridge";

export type RecordState = "idle" | "recording" | "saving";

export interface RecorderState {
	status: RecordState;
	duration: number;
	savedPath: string | null;
	error: string | null;
	bufferSeconds: number;
}

function pad(n: number) { return String(n).padStart(2, "0"); }

/**
 * ShadowPlay-style file naming: "{GameName} {YYYY.MM.DD} - {HH.MM.SS.ff}.DVR.mp4"
 * Example: "Battlefield 6 2026.07.26 - 19.56.14.04.DVR.mp4"
 */
function makeFileName(_label: string, ext: string): string {
	const now = new Date();
	const date = `${now.getFullYear()}.${pad(now.getMonth() + 1)}.${pad(now.getDate())}`;
	const time = `${pad(now.getHours())}.${pad(now.getMinutes())}.${pad(now.getSeconds())}.${pad(Math.floor(now.getMilliseconds() / 10))}`;
	// Use "Desktop" as fallback when no game is detected
	const gameName = (window as any).__clipsta_active_game || "Desktop";
	return `${gameName} ${date} - ${time}.DVR.${ext}`;
}

export function useRecorder(settings: AppSettings | null) {
	const [state, setState] = useState<RecorderState>({
		status: "idle",
		duration: 0,
		savedPath: null,
		error: null,
		bufferSeconds: 0,
	});

	const timerRef = useRef<ReturnType<typeof setInterval> | null>(null);
	const durationRef = useRef(0);
	const wgcActiveRef = useRef(false);

	function startTimer() {
		durationRef.current = 0;
		if (timerRef.current) { clearInterval(timerRef.current); timerRef.current = null; }
		timerRef.current = setInterval(() => {
			durationRef.current += 1;
			setState((s) => ({ ...s, duration: durationRef.current }));
		}, 1000);
	}

	function stopTimer() {
		if (timerRef.current) { clearInterval(timerRef.current); timerRef.current = null; }
		durationRef.current = 0;
	}

	const retryCountRef = useRef(0);
	const startCapture = useCallback(async () => {
		if (wgcActiveRef.current) return;
		try {
			setState((s) => ({ ...s, error: null, status: "recording" }));

			const noAudio = settings?.audioSource === "none" || !(settings?.captureAudio ?? true);
			const micDevice = (settings?.audioSource === "mic" || settings?.audioSource === "both")
				? (settings?.audioInputDeviceId || "default")
				: undefined;
			const loopbackDevice = (settings?.audioSource === "desktop" || settings?.audioSource === "both")
				? (settings?.desktopAudioDeviceId || "default")
				: undefined;

			const result = await bridge.wgcStartRecording({
				sourceId: null,
				fps: settings?.fps ?? 60,
				noAudio,
				micDevice,
				loopbackDevice,
			});

			if (!result) {
				setState((s) => ({ ...s, status: "idle", error: "Capture failed to start" }));
				retryCountRef.current++;
				if (retryCountRef.current < 3) {
					setTimeout(() => { if (!wgcActiveRef.current) startCapture(); }, 2000);
				} else {
					setState((s) => ({ ...s, error: "Capture failed after 3 attempts. Check GPU drivers." }));
				}
				return;
			}

			retryCountRef.current = 0;
			wgcActiveRef.current = true;
			await bridge.setRecordingState(true);
			startTimer();
		} catch (err: any) {
			setState((s) => ({ ...s, status: "idle", error: err.message ?? "Capture failed" }));
			retryCountRef.current++;
			if (retryCountRef.current < 3) {
				setTimeout(() => { if (!wgcActiveRef.current) startCapture(); }, 2000);
			} else {
				setState((s) => ({ ...s, error: "Capture failed after 3 attempts. Check GPU drivers." }));
			}
		}
	}, [settings]);

	const savingRef = useRef(false);
	const saveClip = useCallback(async (seconds: number): Promise<string | null> => {
		if (!wgcActiveRef.current) {
			setState((s) => ({
				...s,
				error: "Recording starting — try again in a moment.",
			}));
			return null;
		}
		// Prevent duplicate saves (hotkey repeat, double-click, etc)
		if (savingRef.current) return null;
		savingRef.current = true;
		try {
			setState((s) => ({ ...s, status: "saving", error: null }));
			const label = seconds <= 30 ? "30sec" : seconds <= 60 ? "1min" : "5min";
			const fileName = makeFileName(label, "mp4");
			const noAudio = settings?.audioSource === "none" || !(settings?.captureAudio ?? true);
			const micDevice = (settings?.audioSource === "mic" || settings?.audioSource === "both")
				? (settings?.audioInputDeviceId || "default")
				: undefined;
			const loopbackDevice = (settings?.audioSource === "desktop" || settings?.audioSource === "both")
				? (settings?.desktopAudioDeviceId || "default")
				: undefined;

			const savedPath = await bridge.wgcSaveClip({
				seconds,
				fileName,
				sourceId: null,
				fps: settings?.fps ?? 60,
				noAudio,
				micDevice,
				loopbackDevice,
			});

			setState((s) => ({
				...s,
				status: "recording",
				savedPath,
				error: savedPath ? null : "Recording — clip will be available in a few seconds.",
			}));
			// Auto-dismiss the "available in a few seconds" message
			if (!savedPath) {
				setTimeout(() => setState((s) => ({ ...s, error: s.error?.includes("available") ? null : s.error })), 3000);
			}
			return savedPath;
		} catch (err: any) {
			setState((s) => ({
				...s,
				status: "recording",
				savedPath: null,
				error: err?.message ?? "Clip save failed",
			}));
			// Auto-dismiss error after 5 seconds
			setTimeout(() => setState((s) => ({ ...s, error: null })), 5000);
			return null;
		} finally {
			savingRef.current = false;
		}
	}, [settings]);

	// Hotkeys via Tauri events
	const saveRef = useRef(saveClip);
	saveRef.current = saveClip;

	useEffect(() => {
		const unlisteners: Promise<() => void>[] = [];

		unlisteners.push(bridge.onHotkeyRecord(() => {}));
		unlisteners.push(bridge.onHotkeyClip1Min(() => saveRef.current(60)));
		unlisteners.push(bridge.onHotkeyClip5Min(() => saveRef.current(300)));
		unlisteners.push(bridge.onHotkeyClip30Sec(() => saveRef.current(30)));

		return () => {
			unlisteners.forEach((p) => p.then((unlisten) => unlisten()).catch(() => {}));
		};
	}, []);

	// Auto-start capture
	const startCaptureRef = useRef(startCapture);
	startCaptureRef.current = startCapture;

	useEffect(() => {
		if (!settings) return;
		if (wgcActiveRef.current) return;

		const timer = setTimeout(() => {
			if (!wgcActiveRef.current) {
				startCaptureRef.current();
			}
		}, 0);
		return () => clearTimeout(timer);
	}, [settings?.fps]);

	// ShadowPlay-style game detection: poll the active window title
	// and store it globally so makeFileName() can use it for clip naming.
	useEffect(() => {
		let active = true;
		const poll = async () => {
			while (active) {
				try {
					const title = await bridge.getActiveWindowTitle();
					(window as any).__clipsta_active_game = title;
				} catch {
					// Ignore errors — keep last known game name
				}
				await new Promise((r) => setTimeout(r, 2000)); // Poll every 2 seconds
			}
		};
		poll();
		return () => { active = false; };
	}, []);

	// WGC clip-saved event
	useEffect(() => {
		const unlistenPromise = bridge.onWgcClipSaved((savedPath: string) => {
			setState((s) => ({ ...s, savedPath }));
		});
		return () => {
			if (unlistenPromise && typeof unlistenPromise === "object" && "then" in unlistenPromise) {
				(unlistenPromise as Promise<() => void>).then((u) => u()).catch(() => {});
			}
		};
	}, []);

	// Clip sound — camera shutter effect
	useEffect(() => {
		const unlistenPromise = bridge.onPlayClipSound(() => {
			try {
				const ctx = new AudioContext();
				const now = ctx.currentTime;

				// Click transient (short impulse)
				const clickBuf = ctx.createBuffer(1, 128, ctx.sampleRate);
				const clickData = clickBuf.getChannelData(0);
				for (let i = 0; i < 128; i++) {
					clickData[i] = (Math.random() * 2 - 1) * Math.exp(-i / 8);
				}
				const click = ctx.createBufferSource();
				click.buffer = clickBuf;
				const clickGain = ctx.createGain();
				clickGain.gain.setValueAtTime(0.6, now);
				clickGain.gain.exponentialRampToValueAtTime(0.001, now + 0.03);
				click.connect(clickGain);
				clickGain.connect(ctx.destination);
				click.start(now);

				// Shutter mechanism sound (filtered noise burst)
				const noiseBuf = ctx.createBuffer(1, ctx.sampleRate * 0.06 | 0, ctx.sampleRate);
				const noiseData = noiseBuf.getChannelData(0);
				for (let i = 0; i < noiseData.length; i++) {
					noiseData[i] = (Math.random() * 2 - 1) * Math.exp(-i / (noiseData.length * 0.15));
				}
				const noise = ctx.createBufferSource();
				noise.buffer = noiseBuf;
				const hp = ctx.createBiquadFilter();
				hp.type = "highpass";
				hp.frequency.value = 2000;
				const noiseGain = ctx.createGain();
				noiseGain.gain.setValueAtTime(0.35, now + 0.01);
				noiseGain.gain.exponentialRampToValueAtTime(0.001, now + 0.07);
				noise.connect(hp);
				hp.connect(noiseGain);
				noiseGain.connect(ctx.destination);
				noise.start(now + 0.01);

				// Cleanup
				setTimeout(() => ctx.close().catch(() => {}), 200);
			} catch { /* ignore */ }
		});
		return () => {
			unlistenPromise.then((u) => u()).catch(() => {});
		};
	}, []);

	// Cleanup on unmount
	useEffect(() => {
		return () => {
			stopTimer();
			if (wgcActiveRef.current) {
				wgcActiveRef.current = false;
				bridge.wgcStopRecording().catch(() => {});
				bridge.setRecordingState(false).catch(() => {});
			}
		};
	}, []);

	return {
		state,
		saveClip,
		isActive: wgcActiveRef.current || state.status === "recording",
	};
}
