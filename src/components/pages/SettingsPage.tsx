import { useCallback, useEffect, useRef, useState } from "react";
import { Save, FolderOpen, RotateCcw, Keyboard, Monitor, Volume2, Cpu, HardDrive, Cloud, Loader2, X, Smartphone, Link2Off } from "lucide-react";
import { QRCodeSVG } from "qrcode.react";
import type { AppSettings } from "../../types";
import { DEFAULTS } from "../../hooks/useSettings";
import type { useCloudUpload } from "../../hooks/useCloudUpload";
import bridge from "../../tauri-bridge";

interface Props {
	settings: AppSettings;
	updateSetting: <K extends keyof AppSettings>(key: K, value: AppSettings[K]) => void;
	saveAll: (s: AppSettings) => Promise<void>;
	cloud: ReturnType<typeof useCloudUpload>;
}

export default function SettingsPage({ settings, updateSetting, saveAll, cloud }: Props) {
	const [local, setLocal] = useState<AppSettings>({ ...settings });
	const [initialSynced, setInitialSynced] = useState(false);
	useEffect(() => {
		if (!initialSynced && settings !== DEFAULTS) { setLocal({ ...settings }); setInitialSynced(true); }
	}, [settings, initialSynced]);

	const [audioInputs, setAudioInputs] = useState<{ deviceId: string; label: string; browserId?: string }[]>([]);
	const [audioOutputs, setAudioOutputs] = useState<{ deviceId: string; label: string }[]>([]);
	const [micLevel, setMicLevel] = useState(0);
	const micPreviewRef = useRef<{ stream: MediaStream; ctx: AudioContext; raf: number } | null>(null);

	useEffect(() => {
		bridge.listAudioDevices().then((wasapiDevices) => {
			navigator.mediaDevices.enumerateDevices().then((browserDevices) => {
				const browserInputByLabel: Record<string, string> = {};
				for (const d of browserDevices) { if (d.kind === "audioinput" && d.label) browserInputByLabel[d.label] = d.deviceId; }
				const inputs = wasapiDevices.filter((d) => d.kind === "input").map((d) => ({ deviceId: d.id, label: d.name || d.id.slice(0, 32), browserId: browserInputByLabel[d.name] ?? "" }));
				const outputs = wasapiDevices.filter((d) => d.kind === "output").map((d) => ({ deviceId: d.id, label: d.name || d.id.slice(0, 32) }));
				setAudioInputs(inputs); setAudioOutputs(outputs);

				// Auto-detect system defaults for first-time setup
				if (!local.desktopAudioDeviceId && !local.audioInputDeviceId) {
					bridge.getDefaultAudioDevices().then((defaults) => {
						if (defaults.defaultOutputId && outputs.some((o) => o.deviceId === defaults.defaultOutputId)) {
							update("desktopAudioDeviceId", defaults.defaultOutputId);
						}
						if (defaults.defaultInputId && inputs.some((i) => i.deviceId === defaults.defaultInputId)) {
							update("audioInputDeviceId", defaults.defaultInputId);
						}
					}).catch(() => {});
				}
			}).catch(() => {});
		}).catch(() => {});
	}, []);

	const startMicPreview = useCallback(async (wasapiId: string) => {
		stopMicPreview();
		const input = audioInputs.find((d) => d.deviceId === wasapiId);
		const browserDeviceId = input?.browserId || "";
		try {
			const stream = await navigator.mediaDevices.getUserMedia({ audio: browserDeviceId ? { deviceId: { exact: browserDeviceId } } : true });
			const ctx = new AudioContext();
			const src = ctx.createMediaStreamSource(stream);
			const analyser = ctx.createAnalyser();
			analyser.fftSize = 256; src.connect(analyser);
			const buf = new Uint8Array(analyser.frequencyBinCount);
			micPreviewRef.current = { stream, ctx, raf: 0 };
			const tick = () => {
				if (!micPreviewRef.current) return;
				analyser.getByteTimeDomainData(buf);
				let max = 0; for (let i = 0; i < buf.length; i++) { const v = Math.abs(buf[i] - 128); if (v > max) max = v; }
				setMicLevel(max / 128);
				micPreviewRef.current.raf = requestAnimationFrame(tick);
			};
			tick();
		} catch { setMicLevel(0); }
	}, [audioInputs]);

	const stopMicPreview = useCallback(() => {
		if (micPreviewRef.current) {
			cancelAnimationFrame(micPreviewRef.current.raf);
			micPreviewRef.current.ctx.close().catch(() => {});
			micPreviewRef.current.stream.getTracks().forEach((t) => t.stop());
			micPreviewRef.current = null;
		}
		setMicLevel(0);
	}, []);

	useEffect(() => {
		const wantMic = local.audioSource === "mic" || local.audioSource === "both";
		if (wantMic && local.audioInputDeviceId) { startMicPreview(local.audioInputDeviceId); }
		else { stopMicPreview(); }
		return () => stopMicPreview();
	}, [local.audioSource, local.audioInputDeviceId, startMicPreview, stopMicPreview]);

	const [saved, setSaved] = useState(false);
	const [showPairingModal, setShowPairingModal] = useState(false);
	const [justPaired, setJustPaired] = useState(false);

	useEffect(() => { if (cloud.pairingConfirmed) { setJustPaired(true); const t = setTimeout(() => setJustPaired(false), 4000); return () => clearTimeout(t); } }, [cloud.pairingConfirmed]);

	const update = <K extends keyof AppSettings>(key: K, value: AppSettings[K]) => { setLocal((prev) => ({ ...prev, [key]: value })); };
	const handleSave = async () => { await saveAll(local); setSaved(true); setTimeout(() => setSaved(false), 2500); };
	const browseFolder = async () => { const folder = await bridge.browseFolder(); if (folder) update("outputFolder", folder); };

	return (
		<div className="h-full overflow-y-auto p-6 max-w-3xl mx-auto">
			<div className="flex items-center justify-between mb-6">
				<div>
					<h1 className="text-2xl font-black text-white">Settings</h1>
					<p className="text-text-dim text-sm mt-0.5">Configure recording, hotkeys and export</p>
				</div>
				<div className="flex items-center gap-3">
					<button onClick={() => setLocal({ ...DEFAULTS })} className="btn-ghost"><RotateCcw size={14} /> Reset Defaults</button>
					<button onClick={handleSave} className="btn-y"><Save size={14} />{saved ? "Saved ✓" : "Save Settings"}</button>
				</div>
			</div>

			<div className="space-y-6">
				{/* Hotkeys */}
				<Section icon={<Keyboard size={16} />} title="Hotkeys">
					<p className="text-text-dim text-xs mb-3">These work globally even when minimized to tray.</p>
					<div className="grid grid-cols-3 gap-4">
						<HotkeyField label="Save Last 30 Seconds" value={local.hotkeyClip30Sec || "Super+Alt+G"} onChange={(v) => update("hotkeyClip30Sec", v)} />
						<HotkeyField label="Save Last 1 Minute" value={local.hotkeyClip1Min} onChange={(v) => update("hotkeyClip1Min", v)} />
						<HotkeyField label="Save Last 5 Minutes" value={local.hotkeyClip5Min} onChange={(v) => update("hotkeyClip5Min", v)} />
					</div>
				</Section>

				{/* Video */}
				<Section icon={<Monitor size={16} />} title="Video">
					<div className="grid grid-cols-2 gap-4">
						<SelectField label="Resolution" value={local.resolution} onChange={(v) => update("resolution", v)} options={["480p", "720p", "1080p", "1440p", "4k"]} />
						<SelectField label="Frame Rate" value={String(local.fps)} onChange={(v) => update("fps", Number(v))} options={["30", "60", "120"]} />
						<SelectField label="Encoder" value={local.encoder} onChange={(v) => update("encoder", v)} options={["auto", "x264 (Software)", "HEVC (H.265)", "NVENC (NVIDIA)", "AMF (AMD)", "QuickSync (Intel)"]} />
						<SelectField label="Aspect Ratio" value={local.aspectRatio} onChange={(v) => update("aspectRatio", v)} options={["16:9", "9:16", "4:3", "21:9"]} />
						<SelectField label="Orientation" value={local.orientation} onChange={(v) => update("orientation", v)} options={["landscape", "portrait"]} display={(v) => v === "landscape" ? "Landscape (1280×720)" : "Portrait (720×1280)"} />
						<NumberField label="Video Bitrate (kbps)" value={local.bitrate} onChange={(v) => update("bitrate", v)} min={1000} max={100000} step={1000} />
						<SelectField label="Buffer Duration" value={String(local.bufferDuration)} onChange={(v) => update("bufferDuration", Number(v))} options={["30", "60", "120", "180", "300"]} display={(v) => v === "60" ? "1 minute" : v === "300" ? "5 minutes" : `${v}s`} />
					</div>
				</Section>

				{/* Audio */}
				<Section icon={<Volume2 size={16} />} title="Audio">
					<div className="grid grid-cols-2 gap-4">
						<SelectField label="Audio Source" value={local.audioSource} onChange={(v) => update("audioSource", v)} options={["desktop", "mic", "both", "none"]} display={(v) => v === "desktop" ? "Desktop Audio" : v === "mic" ? "Microphone only" : v === "both" ? "Desktop + Microphone" : "None"} />
						<NumberField label="Audio Bitrate (kbps)" value={local.audioBitrate} onChange={(v) => update("audioBitrate", v)} min={64} max={320} step={32} />
						{(local.audioSource === "mic" || local.audioSource === "both") && (
							<>
								<SelectField label="Microphone" value={local.audioInputDeviceId} onChange={(v) => { update("audioInputDeviceId", v); if (v) startMicPreview(v); else stopMicPreview(); }} options={["", ...audioInputs.map((d) => d.deviceId)]} display={(v) => v === "" ? "System Default" : audioInputs.find((d) => d.deviceId === v)?.label ?? v.slice(0, 16)} />
								{local.audioInputDeviceId && (
									<div className="space-y-1 col-span-2">
										<p className="label text-[10px]">Mic Level</p>
										<div className="h-2 bg-muted rounded-full overflow-hidden">
											<div className="h-full rounded-full transition-all duration-75" style={{ width: `${Math.min(micLevel * 100, 100)}%`, backgroundColor: micLevel > 0.7 ? "#ef4444" : micLevel > 0.4 ? "#f59e0b" : "#D4F000" }} />
										</div>
									</div>
								)}
							</>
						)}
						{(local.audioSource === "desktop" || local.audioSource === "both") && (
							<SelectField label="Desktop Audio Device" value={local.desktopAudioDeviceId} onChange={(v) => update("desktopAudioDeviceId", v)} options={["", ...audioOutputs.map((d) => d.deviceId)]} display={(v) => v === "" ? "System Default" : audioOutputs.find((d) => d.deviceId === v)?.label ?? v.slice(0, 16)} />
						)}
					</div>
				</Section>

				{/* Capture */}
				<Section icon={<Cpu size={16} />} title="Capture Behavior">
					<div className="grid grid-cols-2 gap-4">
						<Toggle label="Auto Game Detection" checked={local.gameDetect} onChange={(v) => update("gameDetect", v)} description="Automatically capture the active fullscreen game" />
						<Toggle label="Minimize to System Tray" checked={local.minimizeToTray} onChange={(v) => update("minimizeToTray", v)} description="Keep running in tray when window is closed" />
						<Toggle label="Show Overlay" checked={local.overlayEnabled} onChange={(v) => update("overlayEnabled", v)} description="Show notification on clip save" />
						<Toggle label="Clip Sound" checked={local.clipSoundEnabled ?? true} onChange={(v) => update("clipSoundEnabled", v)} description="Play a sound when a clip is saved" />
					</div>
				</Section>

				{/* Cloud */}
				<Section icon={<Cloud size={16} />} title="Cloud Upload">
					<div className="grid grid-cols-2 gap-4">
						<Toggle label="Enable Cloud Upload" checked={local.cloudEnabled} onChange={(v) => update("cloudEnabled", v)} description="Upload clips to your paired mobile device" />
						<Toggle label="Auto-Upload New Clips" checked={local.autoUpload} onChange={(v) => update("autoUpload", v)} description="Automatically queue new recordings" />
						<NumberField label="Upload Bandwidth (KB/s)" value={local.uploadBandwidth} onChange={(v) => update("uploadBandwidth", v)} min={0} max={50000} step={500} />
						<Toggle label="Delete After Upload" checked={local.deleteAfterUpload} onChange={(v) => update("deleteAfterUpload", v)} description="Remove local file after upload" />
					</div>
					{local.cloudEnabled && (
						<div className="bg-muted rounded-lg p-3 space-y-2">
							<div className="flex items-center justify-between">
								<p className="text-xs text-text-dim font-semibold">Pairing</p>
								{cloud.pairingConfirmed ? <span className={`text-[10px] text-green-400 font-bold ${justPaired ? "animate-pulse" : ""}`}>✓ Paired</span> : cloud.paired ? <span className="text-[10px] text-y font-bold">Pending</span> : <span className="text-[10px] text-text-dim">Not paired</span>}
							</div>
							{justPaired && <div className="flex items-center gap-2 text-green-400 text-xs py-1"><div className="w-5 h-5 rounded-full bg-green-500 flex items-center justify-center"><span className="text-black text-[10px] font-bold">✓</span></div><span>Paired successfully!</span></div>}
							<button onClick={() => { setShowPairingModal(true); cloud.generatePairingCode(); }} className="btn-y w-full justify-center text-xs py-2"><Smartphone size={12} /> Pair Mobile Device</button>
							{cloud.paired && <button onClick={() => { cloud.clearPairing(); setShowPairingModal(false); }} className="btn-ghost w-full justify-center text-xs py-2 text-red-400 hover:text-red-300"><Link2Off size={12} /> Unpair Device</button>}
						</div>
					)}
				</Section>

				{/* Watch Folder */}
				<Section icon={<FolderOpen size={16} />} title="Watch Folder">
					<p className="text-text-dim text-xs mb-3">Monitor a folder for video files from other recording software. Detected files will be automatically queued for upload.</p>
					<div className="grid grid-cols-2 gap-4">
						<Toggle label="Enable Watch Folder" checked={local.watchFolderEnabled} onChange={(v) => update("watchFolderEnabled", v)} description="Monitor folder for new MP4/MOV/MKV files" />
					</div>
					{local.watchFolderEnabled && (
						<div className="space-y-1 mt-3">
							<p className="label">Watch Folder Path</p>
							<div className="flex gap-2">
								<input value={local.watchFolderPath} onChange={(e) => update("watchFolderPath", e.target.value)} className="input flex-1" placeholder="C:\Users\...\Videos\OBS" />
								<button onClick={async () => { const folder = await bridge.browseFolder(); if (folder) update("watchFolderPath", folder); }} className="btn-ghost flex-shrink-0"><FolderOpen size={14} /> Browse</button>
							</div>
						</div>
					)}
				</Section>

				{/* Output */}
				<Section icon={<HardDrive size={16} />} title="Output">
					<div className="space-y-1">
						<p className="label">Clips Folder</p>
						<div className="flex gap-2">
							<input value={local.outputFolder} onChange={(e) => update("outputFolder", e.target.value)} className="input flex-1" placeholder="C:\Users\...\Videos\Clipsta" />
							<button onClick={browseFolder} className="btn-ghost flex-shrink-0"><FolderOpen size={14} /> Browse</button>
							<button onClick={() => bridge.openFolder(local.outputFolder)} className="btn-ghost flex-shrink-0">Open</button>
						</div>
					</div>
				</Section>
			</div>

			{/* Save bar */}
			<div className="sticky bottom-0 pt-6 pb-2 bg-gradient-to-t from-bg to-transparent">
				<button onClick={handleSave} className="btn-y w-full justify-center py-3 text-base"><Save size={18} />{saved ? "Settings Saved ✓" : "Save All Settings"}</button>
			</div>

			{/* Pairing QR modal */}
			{showPairingModal && (
				<div className="fixed inset-0 z-50 flex items-center justify-center bg-black/70" onClick={() => setShowPairingModal(false)}>
					<div className="bg-card rounded-xl p-8 max-w-sm w-full mx-4 shadow-2xl border border-border" onClick={(e) => e.stopPropagation()}>
						<div className="flex items-center justify-between mb-4">
							<h3 className="text-white font-bold text-lg">Pair Mobile Device</h3>
							<button onClick={() => setShowPairingModal(false)} className="text-text-dim hover:text-white transition-colors"><X size={18} /></button>
						</div>
						{cloud.pairingLoading && <div className="text-center py-10"><Loader2 size={32} className="animate-spin text-y mx-auto mb-3" /><p className="text-text-dim text-sm">Generating pairing code…</p></div>}
						{cloud.pairingUrl && !cloud.pairingLoading && (
							<div className="text-center space-y-4">
								<div className="flex justify-center">
									<div className="w-[200px] h-[200px] bg-white rounded-lg flex items-center justify-center p-3">
										<QRCodeSVG value={cloud.pairingUrl} size={176} bgColor="#ffffff" fgColor="#000000" level="M" />
									</div>
								</div>
								<p className="text-text-dim text-xs">Open Clipsta on your iPhone and scan this QR code</p>
								<button onClick={() => { cloud.confirmPairing(); setShowPairingModal(false); const updatedLocal = { ...local, cloudPairCode: cloud.pairingCode ?? "" }; setLocal(updatedLocal); saveAll(updatedLocal); }} className="btn-y w-full justify-center text-xs py-2 mt-2">Done — I've scanned the code</button>
							</div>
						)}
						{cloud.pairingError && !cloud.pairingLoading && (
							<div className="text-center py-6 space-y-3">
								<p className="text-red-400 text-sm">{cloud.pairingError}</p>
								<button onClick={() => cloud.generatePairingCode()} className="btn-y w-full justify-center text-xs py-2">Retry</button>
							</div>
						)}
					</div>
				</div>
			)}
		</div>
	);
}


// ── Helper Components ───────────────────────────────────────────────────────
function Section({ icon, title, children }: { icon: React.ReactNode; title: string; children: React.ReactNode }) {
	return (
		<div className="card p-5 space-y-4">
			<div className="flex items-center gap-2 border-b border-border pb-3">
				<span className="text-y">{icon}</span>
				<h3 className="font-bold text-white">{title}</h3>
			</div>
			{children}
		</div>
	);
}

function HotkeyField({ label, value, onChange }: { label: string; value: string; onChange: (v: string) => void }) {
	const [capturing, setCapturing] = useState(false);
	const [keys, setKeys] = useState<string[]>([]);

	const startCapture = () => { bridge.suspendHotkeys().catch(() => {}); setCapturing(true); setKeys([]); };
	const endCapture = () => { setCapturing(false); bridge.resumeHotkeys().catch(() => {}); };

	const onKeyDown = (e: React.KeyboardEvent) => {
		e.preventDefault(); e.stopPropagation();
		const parts: string[] = [];
		if (e.ctrlKey) parts.push("Ctrl");
		if (e.altKey) parts.push("Alt");
		if (e.shiftKey) parts.push("Shift");
		if (e.metaKey) parts.push("Super");
		if (!["Control", "Alt", "Shift", "Meta"].includes(e.key)) parts.push(e.key.length === 1 ? e.key.toUpperCase() : e.key);
		setKeys(parts);
		if (!["Control", "Alt", "Shift", "Meta"].includes(e.key)) { onChange(parts.join("+")); endCapture(); }
	};

	return (
		<div className="space-y-1">
			<p className="label">{label}</p>
			{capturing ? (
				<input autoFocus className="input font-mono text-y text-center" placeholder="Press keys…" value={keys.join("+")} onKeyDown={onKeyDown} onBlur={endCapture} readOnly />
			) : (
				<button onClick={startCapture} className="input font-mono text-y text-center cursor-pointer hover:border-y w-full">{value || "Click to set"}</button>
			)}
		</div>
	);
}

function SelectField({ label, value, onChange, options, display }: { label: string; value: string; onChange: (v: string) => void; options: string[]; display?: (v: string) => string }) {
	return (
		<div className="space-y-1">
			<p className="label">{label}</p>
			<select value={value} onChange={(e) => onChange(e.target.value)} className="input no-drag">
				{options.map((o) => <option key={o} value={o}>{display ? display(o) : o}</option>)}
			</select>
		</div>
	);
}

function NumberField({ label, value, onChange, min, max, step }: { label: string; value: number; onChange: (v: number) => void; min: number; max: number; step: number }) {
	return (
		<div className="space-y-1">
			<p className="label">{label}</p>
			<div className="flex items-center gap-2">
				<input type="range" min={min} max={max} step={step} value={value} onChange={(e) => onChange(Number(e.target.value))} className="flex-1 accent-[#D4F000] no-drag" />
				<span className="text-white text-sm font-mono w-16 text-right">{value.toLocaleString()}</span>
			</div>
		</div>
	);
}

function Toggle({ label, checked, onChange, description }: { label: string; checked: boolean; onChange: (v: boolean) => void; description?: string }) {
	return (
		<label className="flex items-start gap-3 cursor-pointer group">
			<div onClick={() => onChange(!checked)} className={`mt-0.5 w-10 h-5 rounded-full flex-shrink-0 transition-colors relative cursor-pointer ${checked ? "bg-y" : "bg-muted"}`}>
				<div className={`absolute top-0.5 w-4 h-4 rounded-full bg-black transition-transform ${checked ? "translate-x-5" : "translate-x-0.5"}`} />
			</div>
			<div>
				<p className="text-sm text-white font-medium group-hover:text-y transition-colors">{label}</p>
				{description && <p className="text-text-dim text-xs mt-0.5">{description}</p>}
			</div>
		</label>
	);
}
