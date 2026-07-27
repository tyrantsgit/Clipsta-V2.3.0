import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
	Scissors, Download, RotateCcw, Volume2, VolumeX, FileUp,
	Play, Pause, SkipBack, SkipForward, Crop, Loader2, Trash2, FolderOpen, Upload, Pen, Check,
	Plus, ArrowUp, ArrowDown, GripHorizontal,
} from "lucide-react";
import type { AppSettings, ExportOpts, TimelineEntry } from "../../types";
import type { useCloudUpload } from "../../hooks/useCloudUpload";
import { toFileUrl, formatTime, sanitizeName, getTimeFromEvent } from "../../utils";
import { getCurrentWindow } from "@tauri-apps/api/window";
import bridge from "../../tauri-bridge";

interface Props {
	initialFile: string | null;
	settings: AppSettings;
	cloud: ReturnType<typeof useCloudUpload>;
}

interface CutSeg { start: number; end: number }

export default function EditorPage({ initialFile, settings, cloud }: Props) {
	const [timeline, setTimeline] = useState<TimelineEntry[]>(() => {
		if (!initialFile) return [];
		const name = initialFile.replace(/^.*[\\/]/, "");
		return [{ id: crypto.randomUUID(), path: initialFile, name, trimIn: 0, trimOut: 0 }];
	});
	const [activeIdx, setActiveIdx] = useState(0);
	const filePath = timeline[activeIdx]?.path ?? null;

	const [playing, setPlaying] = useState(false);
	const [currentTime, setCurrentTime] = useState(0);
	const [duration, setDuration] = useState(0);
	const [volume, setVolume] = useState(1);
	const [muted, setMuted] = useState(false);
	const [trimIn, setTrimIn] = useState(0);
	const [trimOut, setTrimOut] = useState(0);
	const [cuts, setCuts] = useState<CutSeg[]>([]);
	const [exporting, setExporting] = useState(false);
	const [exportDone, setExportDone] = useState<string | null>(null);
	const [exportError, setExportError] = useState<string | null>(null);
	const videoRef = useRef<HTMLVideoElement>(null);
	const [timelineScale, setTimelineScale] = useState(1);
	const [scissorPos, setScissorPos] = useState<number | null>(null);
	const [cutMode, setCutMode] = useState(false);
	const cutCount = cuts.length;
	const [dragOver, setDragOver] = useState(false);
	const dragSrcRef = useRef<number | null>(null);
	const uploadJob = filePath ? cloud.queue.find((j) => j.path === filePath) : undefined;
	const uploadBusy = !!(uploadJob && (uploadJob.status === "queued" || uploadJob.status === "uploading"));
	const [showUploaded, setShowUploaded] = useState(false);

	useEffect(() => {
		if (uploadJob?.status === "done") {
			setShowUploaded(true);
			const t = setTimeout(() => setShowUploaded(false), 2000);
			return () => clearTimeout(t);
		}
	}, [uploadJob?.status]);

	const [renaming, setRenaming] = useState(false);
	const [renameValue, setRenameValue] = useState("");
	const renameInputRef = useRef<HTMLInputElement>(null);
	const [resetKey, setResetKey] = useState(0);
	const renameRef = useRef<HTMLDivElement>(null);

	const handleRename = async () => {
		const entry = timeline[activeIdx];
		if (!entry || !renameValue.trim()) return;
		const base = renameValue.trim();
		const ext = entry.path.replace(/^.*\./, "");
		const newName = ext ? `${base}.${ext}` : base;
		setTimeline((prev) => prev.map((e, i) => i === activeIdx ? { ...e, name: newName } : e));
		setRenaming(false);
	};

	const cancelRename = () => { setRenaming(false); };

	const [expFormat, setExpFormat] = useState("mp4");
	const [expResolution, setExpResolution] = useState(settings.resolution);
	const [expAspect, setExpAspect] = useState(settings.aspectRatio);

	const selectClip = useCallback((idx: number) => {
		if (idx === activeIdx || idx < 0 || idx >= timeline.length) return;
		setTimeline((prev) => prev.map((e, i) => i === activeIdx ? { ...e, trimIn, trimOut } : e));
		setActiveIdx(idx);
	}, [activeIdx, trimIn, trimOut, timeline.length]);

	useEffect(() => {
		const entry = timeline[activeIdx];
		if (entry) {
			setTrimIn(entry.trimIn);
			if (entry.trimOut > 0) setTrimOut(entry.trimOut);
			setCurrentTime(0);
			setDuration(0);
			setCuts([]);
			setCutMode(false);
			setRenaming(false);
			setExportDone(null);
			setExportError(null);
			setPlaying(false);
		}
	}, [activeIdx, resetKey]); // eslint-disable-line react-hooks/exhaustive-deps

	useEffect(() => {
		const entry = timeline[activeIdx];
		if (entry && (entry.trimIn !== trimIn || entry.trimOut !== trimOut)) {
			setTimeline((prev) => prev.map((e, i) => i === activeIdx ? { ...e, trimIn, trimOut } : e));
		}
	}, [trimIn, trimOut, activeIdx, timeline]);

	const addToTimeline = useCallback(async (path: string) => {
		const name = path.replace(/^.*[\\/]/, "");
		const entry: TimelineEntry = { id: crypto.randomUUID(), path, name, trimIn: 0, trimOut: 0 };
		setTimeline((prev) => [...prev, entry]);
		setActiveIdx(timeline.length);
	}, [timeline.length]);

	// Tauri 2 native drag-drop: provides full file paths even in WebView2
	const addToTimelineRef = useRef(addToTimeline);
	addToTimelineRef.current = addToTimeline;
	useEffect(() => {
		const unlisten = getCurrentWindow().onDragDropEvent((event) => {
			if (event.payload.type === "drop") {
				const paths = (event.payload.paths ?? []).filter((p: string) =>
					/\.(webm|mp4|mkv|mov)$/i.test(p)
				);
				paths.forEach((p: string) => addToTimelineRef.current(p));
			}
		});
		return () => { unlisten.then((u) => u()); };
	}, []);

	const removeFromTimeline = useCallback((idx: number) => {
		const newTimeline = timeline.filter((_, i) => i !== idx);
		setTimeline(newTimeline);
		if (newTimeline.length === 0) {
			setActiveIdx(-1); setCurrentTime(0); setDuration(0);
			setTrimIn(0); setTrimOut(0); setCuts([]); setCutMode(false); setPlaying(false);
		} else if (activeIdx >= newTimeline.length) {
			setActiveIdx(newTimeline.length - 1);
		} else if (activeIdx > idx) {
			setActiveIdx(activeIdx - 1);
		} else if (activeIdx === idx) {
			setActiveIdx(Math.min(idx, newTimeline.length - 1));
		}
	}, [timeline, activeIdx]);

	const moveClip = useCallback((idx: number, dir: -1 | 1) => {
		const to = idx + dir;
		if (to < 0 || to >= timeline.length) return;
		setTimeline((prev) => {
			const next = [...prev];
			[next[idx], next[to]] = [next[to], next[idx]];
			return next;
		});
		setActiveIdx(to);
	}, [timeline.length]);

	const prevInitialRef = useRef(initialFile);
	useEffect(() => {
		if (initialFile && initialFile !== prevInitialRef.current) {
			prevInitialRef.current = initialFile;
			const name = initialFile.replace(/^.*[\\/]/, "");
			setTimeline([{ id: crypto.randomUUID(), path: initialFile, name, trimIn: 0, trimOut: 0 }]);
			setActiveIdx(0); setCurrentTime(0); setDuration(0);
			setCuts([]); setExportDone(null); setExportError(null); setPlaying(false);
		}
	}, [initialFile]);

	const onLoadedMetadata = () => {
		const v = videoRef.current;
		if (!v || !isFinite(v.duration)) return;
		setDuration((prev) => prev > 0 ? prev : v.duration);
		const entry = timeline[activeIdx];
		if (entry && entry.trimOut === 0) {
			setTrimOut(v.duration);
			setTimeline((prev) => prev.map((e, i) => i === activeIdx ? { ...e, trimOut: v.duration } : e));
		}
	};

	const onTimeUpdate = () => {
		const v = videoRef.current;
		if (!v) return;
		setCurrentTime(v.currentTime);
		if (trimOut > 0 && v.currentTime >= trimOut) {
			v.currentTime = trimIn;
			if (!playing) v.pause();
		}
	};

	const seek = (t: number) => {
		if (videoRef.current) videoRef.current.currentTime = t;
		setCurrentTime(t);
	};

	const scrubRef = useRef(false);

	const handleTimelineMouseDown = (e: React.MouseEvent) => {
		const parent = e.currentTarget;
		const t = Math.max(0, Math.min(duration, getTimeFromEvent(e.clientX, parent, duration)));
		if (cutMode) {
			setScissorPos(t);
			setTimeout(() => setScissorPos(null), 600);
			const half = 0.5;
			const start = Math.max(0, t - half);
			const end = Math.min(duration, t + half);
			setCuts((prev) => [...prev, { start, end }].sort((a, b) => a.start - b.start));
			return;
		}
		seek(t);
		scrubRef.current = true;
		const move = (me: MouseEvent) => {
			if (!scrubRef.current) return;
			const t2 = Math.max(0, Math.min(duration, getTimeFromEvent(me.clientX, parent, duration)));
			seek(t2);
		};
		const up = () => {
			scrubRef.current = false;
			window.removeEventListener("mousemove", move);
			window.removeEventListener("mouseup", up);
		};
		window.addEventListener("mousemove", move);
		window.addEventListener("mouseup", up);
	};

	const handleTimelineContext = (e: React.MouseEvent) => {
		e.preventDefault();
		const parent = e.currentTarget as HTMLElement;
		const t = Math.max(0, Math.min(duration, getTimeFromEvent(e.clientX, parent, duration)));
		setScissorPos(t);
		setTimeout(() => setScissorPos(null), 600);
		const half = 0.5;
		const start = Math.max(0, t - half);
		const end = Math.min(duration, t + half);
		setCuts((prev) => [...prev, { start, end }].sort((a, b) => a.start - b.start));
	};

	const togglePlay = () => {
		const v = videoRef.current;
		if (!v) return;
		if (playing) { v.pause(); setPlaying(false); }
		else {
			if (v.currentTime >= trimOut || v.currentTime < trimIn) v.currentTime = trimIn;
			v.play().catch(() => setExportError("Failed to play video file"));
			setPlaying(true);
		}
	};

	const setVol = (val: number) => {
		setVolume(val);
		if (videoRef.current) videoRef.current.volume = val;
	};

	const cutAtPlayhead = () => {
		const half = 0.5;
		const t = videoRef.current?.currentTime ?? currentTime;
		const start = Math.max(0, t - half);
		const end = Math.min(duration, t + half);
		setCuts((prev) => [...prev, { start, end }].sort((a, b) => a.start - b.start));
	};

	const removeCut = (idx: number) => { setCuts((prev) => prev.filter((_, i) => i !== idx)); };

	const pct = useCallback((t: number) => duration > 0 ? (t / duration) * 100 : 0, [duration]);

	// Keyboard shortcuts
	const togglePlayRef = useRef(togglePlay);
	togglePlayRef.current = togglePlay;
	const cutAtPlayheadRef = useRef(cutAtPlayhead);
	cutAtPlayheadRef.current = cutAtPlayhead;

	useEffect(() => {
		const onKey = (e: KeyboardEvent) => {
			if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement || e.target instanceof HTMLTextAreaElement) return;
			const t = videoRef.current?.currentTime ?? 0;
			switch (e.code) {
				case "Space": e.preventDefault(); togglePlayRef.current(); break;
				case "KeyI": setTrimIn(t); if (videoRef.current) videoRef.current.currentTime = t; break;
				case "KeyO": setTrimOut(t); break;
				case "KeyX": e.preventDefault(); cutAtPlayheadRef.current(); break;
				case "Escape": setCutMode(false); break;
			}
		};
		window.addEventListener("keydown", onKey);
		return () => window.removeEventListener("keydown", onKey);
	}, []);

	// Drag-and-drop
	const dragCounterRef = useRef(0);

	const getDroppedPathsLocal = (dt: DataTransfer): string[] => {
		const paths: string[] = [];
		for (const f of Array.from(dt.files)) {
			const p = (f as any).path || f.name;
			if (p && /\.(webm|mp4|mkv|mov)$/i.test(p)) paths.push(p);
		}
		return paths;
	};

	const handleDrop = (e: React.DragEvent) => {
		e.preventDefault();
		dragCounterRef.current = 0;
		setDragOver(false);
		const paths = getDroppedPathsLocal(e.dataTransfer);
		paths.forEach((p) => addToTimeline(p));
	};

	const handleDragEnter = (e: React.DragEvent) => { e.preventDefault(); dragCounterRef.current++; setDragOver(true); };
	const handleDragOver = (e: React.DragEvent) => { e.preventDefault(); };
	const handleDragLeave = (e: React.DragEvent) => {
		e.preventDefault();
		dragCounterRef.current--;
		if (dragCounterRef.current <= 0) { dragCounterRef.current = 0; setDragOver(false); }
	};

	const handleBrowse = async () => {
		const path = await bridge.browseFile();
		if (path) addToTimeline(path);
	};

	const handleQuickMerge = async () => {
		if (timeline.length < 2) return;
		setPlaying(false);
		if (videoRef.current) videoRef.current.pause();
		const exportTimeline = timeline.map((e, i) => i === activeIdx ? { ...e, trimIn, trimOut } : e);
		const zeroDur = exportTimeline.find((e) => e.trimOut <= e.trimIn);
		if (zeroDur) { setExportError(`"${zeroDur.name}" has no duration loaded yet.`); return; }
		const folder = settings.outputFolder;
		if (!folder) { setExportError("No output folder set in Settings"); return; }
		const mergedName = exportTimeline.map((e) => sanitizeName(e.name.replace(/\.[^.]+$/, ""))).join(" + ");
		const outPath = `${folder}\\${mergedName}.mp4`;
		try { await bridge.ensureDir(folder); } catch { /* best effort */ }
		setExporting(true); setExportDone(null); setExportError(null);
		try {
			const primaryPath = exportTimeline[0].path;
			const opts: ExportOpts = {
				format: "mp4", resolution: expResolution, aspectRatio: expAspect,
				fps: settings.fps, encoder: settings.encoder,
				timeline: exportTimeline.map((e) => ({ path: e.path, trimIn: e.trimIn, trimOut: e.trimOut })),
			};
			const out = await bridge.exportRecording(primaryPath, outPath, opts);
			if (out) {
				const name = out.replace(/^.*[\\/]/, "");
				setTimeline([{ id: crypto.randomUUID(), path: out, name, trimIn: 0, trimOut: 0 }]);
				setActiveIdx(0); setTrimIn(0); setTrimOut(0); setCurrentTime(0); setDuration(0);
				setCuts([]); setCutMode(false); setScissorPos(null);
				setExportDone(out); setExportError(null); setPlaying(false); setResetKey((k) => k + 1);
			} else { setExportError("Merge failed — no output file was created"); }
		} catch (e: any) { setExportError(e?.message ?? String(e)); }
		finally { setExporting(false); }
	};

	const handleExport = async () => {
		if (timeline.length === 0) return;
		setPlaying(false);
		if (videoRef.current) videoRef.current.pause();
		const exportTimeline = timeline.map((e, i) => i === activeIdx ? { ...e, trimIn, trimOut } : e);
		const baseName = exportTimeline.length === 1
			? (exportTimeline[0].name || exportTimeline[0].path.replace(/^.*[\\/]/, "")).replace(/\.[^.]+$/, "")
			: "combined";
		const ext = expFormat === "webm" ? "webm" : expFormat === "mkv" ? "mkv" : expFormat === "mov" ? "mov" : "mp4";
		const savePath = await bridge.browseSaveExport(`${baseName}_export.${ext}`);
		if (!savePath) return;
		setExporting(true); setExportDone(null); setExportError(null);
		try {
			const primaryPath = exportTimeline[0].path;
			const exportingIdx = activeIdx;
			const opts: ExportOpts = {
				format: expFormat, resolution: expResolution, aspectRatio: expAspect,
				fps: settings.fps, encoder: settings.encoder,
				timeline: exportTimeline.length > 1 ? exportTimeline.map((e) => ({ path: e.path, trimIn: e.trimIn, trimOut: e.trimOut })) : undefined,
				trimStart: exportTimeline.length === 1 && trimIn > 0 ? trimIn : undefined,
				trimEnd: exportTimeline.length === 1 && trimOut < duration ? trimOut : undefined,
				cuts: exportTimeline.length === 1 && cuts.length > 0 ? cuts : undefined,
			};
			const out = await bridge.exportRecording(primaryPath, savePath, opts);
			setExportDone(out ?? null);
			if (out) {
				const name = out.replace(/^.*[\\/]/, "");
				if (exportTimeline.length > 1) {
					setTimeline([{ id: crypto.randomUUID(), path: out, name, trimIn: 0, trimOut: 0 }]);
					setActiveIdx(0);
				} else {
					setTimeline((prev) => prev.map((e, i) => i === exportingIdx ? { ...e, path: out, name, trimIn: 0, trimOut: 0 } : e));
				}
				setTrimIn(0); setTrimOut(0); setCurrentTime(0); setDuration(0);
				setCuts([]); setCutMode(false); setScissorPos(null); setExportError(null); setPlaying(false);
				setResetKey((k) => k + 1);
			} else { setExportError("Export failed — no output file was created"); }
		} catch (e: any) { setExportError(e?.message ?? String(e)); }
		finally { setExporting(false); }
	};

	const ticks = useMemo(() => {
		const tickInterval = Math.max(1, Math.floor(10 / timelineScale));
		const result: number[] = [];
		if (duration > 0 && isFinite(duration)) {
			const maxTicks = 10000;
			for (let t = 0, count = 0; t <= duration && count < maxTicks; t += tickInterval, count++) result.push(t);
		}
		return result;
	}, [duration, timelineScale]);

	// Empty state
	if (!filePath) {
		return (
			<div
				className={`flex-1 flex items-center justify-center flex-col gap-4 text-center p-8 transition-colors ${dragOver ? "bg-[#1c1c00]" : ""}`}
				onDrop={handleDrop} onDragEnter={handleDragEnter} onDragOver={handleDragOver} onDragLeave={handleDragLeave}
			>
				<div className={`rounded-2xl border-2 border-dashed p-12 flex flex-col items-center gap-4 transition-all duration-200 ${dragOver ? "border-y bg-[#2a2a00] scale-105 shadow-[0_0_40px_rgba(212,240,0,0.15)]" : "border-border hover:border-text-dim"}`}>
					<Scissors size={48} className={`transition-colors ${dragOver ? "text-y" : "text-text-dim opacity-30"}`} />
					<div>
						<p className={`text-xl font-bold transition-colors ${dragOver ? "text-y" : "text-white"}`}>
							{dragOver ? "Drop to start editing" : "Drop a video here"}
						</p>
						<p className="text-text-dim text-sm mt-1">or open from the Library to start editing</p>
					</div>
					<button onClick={handleBrowse} className="btn-y mt-2"><FolderOpen size={14} /> Browse Files</button>
				</div>
			</div>
		);
	}

	return (
		<EditorLayout
			timeline={timeline} activeIdx={activeIdx} filePath={filePath}
			selectClip={selectClip} addToTimeline={addToTimeline} removeFromTimeline={removeFromTimeline}
			moveClip={moveClip} handleBrowse={handleBrowse} handleDrop={handleDrop}
			handleDragEnter={handleDragEnter} handleDragOver={handleDragOver} handleDragLeave={handleDragLeave}
			dragOver={dragOver} dragSrcRef={dragSrcRef} setTimeline={setTimeline} setActiveIdx={setActiveIdx}
			videoRef={videoRef} onLoadedMetadata={onLoadedMetadata} onTimeUpdate={onTimeUpdate}
			playing={playing} setPlaying={setPlaying} exportError={exportError} setExportError={setExportError}
			muted={muted} setMuted={setMuted} volume={volume} setVol={setVol}
			currentTime={currentTime} duration={duration} trimIn={trimIn} trimOut={trimOut}
			setTrimIn={setTrimIn} setTrimOut={setTrimOut} seek={seek}
			cuts={cuts} setCuts={setCuts} cutMode={cutMode} setCutMode={setCutMode}
			cutCount={cutCount} scissorPos={scissorPos} ticks={ticks} pct={pct}
			handleTimelineMouseDown={handleTimelineMouseDown} handleTimelineContext={handleTimelineContext}
			togglePlay={togglePlay} timelineScale={timelineScale} setTimelineScale={setTimelineScale}
			cutAtPlayhead={cutAtPlayhead} removeCut={removeCut}
			renaming={renaming} setRenaming={setRenaming} renameValue={renameValue} setRenameValue={setRenameValue}
			renameInputRef={renameInputRef} renameRef={renameRef} handleRename={handleRename} cancelRename={cancelRename}
			handleQuickMerge={handleQuickMerge} handleExport={handleExport}
			exporting={exporting} exportDone={exportDone}
			expFormat={expFormat} setExpFormat={setExpFormat}
			expResolution={expResolution} setExpResolution={setExpResolution}
			expAspect={expAspect} setExpAspect={setExpAspect}
			cloud={cloud} uploadJob={uploadJob} uploadBusy={uploadBusy} showUploaded={showUploaded}
			settings={settings}
		/>
	);
}


// ── EditorLayout: main editing UI ───────────────────────────────────────────
function EditorLayout(props: any) {
	const {
		timeline, activeIdx, filePath,
		selectClip, removeFromTimeline, moveClip, handleBrowse,
		handleDrop, handleDragEnter, handleDragOver, handleDragLeave,
		dragOver, dragSrcRef, setTimeline, setActiveIdx,
		videoRef, onLoadedMetadata, onTimeUpdate,
		playing, setPlaying, exportError, setExportError,
		muted, setMuted, volume, setVol,
		currentTime, duration, trimIn, trimOut,
		setTrimIn, setTrimOut, seek,
		cuts, setCuts, cutMode, setCutMode,
		cutCount, scissorPos, ticks, pct,
		handleTimelineMouseDown, handleTimelineContext,
		togglePlay, timelineScale, setTimelineScale,
		cutAtPlayhead, removeCut,
		renaming, setRenaming, renameValue, setRenameValue,
		renameInputRef, renameRef, handleRename, cancelRename,
		handleQuickMerge, handleExport,
		exporting, exportDone,
		expFormat, setExpFormat,
		expResolution, setExpResolution,
		expAspect, setExpAspect,
		cloud, uploadJob, uploadBusy, showUploaded,
		settings,
	} = props;

	return (
		<div
			className={`flex-1 flex overflow-hidden transition-colors ${dragOver ? "bg-[#1c1c00]" : ""}`}
			onDrop={handleDrop} onDragEnter={handleDragEnter} onDragOver={handleDragOver} onDragLeave={handleDragLeave}
		>
			{dragOver && (
				<div className="absolute inset-0 z-50 flex items-center justify-center bg-black/70 pointer-events-none">
					<div className="rounded-2xl border-2 border-dashed border-y p-10 text-center bg-[#1c1c00]/80 backdrop-blur-sm">
						<FileUp size={40} className="mx-auto mb-3 text-y" />
						<p className="text-y text-lg font-bold">{timeline.length > 0 ? "Drop to add to timeline" : "Drop video to start"}</p>
						<p className="text-text-dim text-sm mt-1">Supports MP4, WebM, MKV, MOV</p>
					</div>
				</div>
			)}

			{/* Main editor area */}
			<div className="flex-1 flex flex-col overflow-hidden">
				{/* Clip strip */}
				{timeline.length > 0 && (
					<div className="flex-shrink-0 bg-[#0d0d0d] border-b border-border px-3 py-2">
						<div className="flex gap-2 items-center overflow-x-auto">
							{timeline.map((clip: any, idx: number) => (
								<div
									key={clip.id}
									onClick={() => selectClip(idx)}
									draggable={timeline.length > 1}
									onDragStart={(e: React.DragEvent) => {
										e.dataTransfer.effectAllowed = "move";
										e.dataTransfer.setData("text/plain", String(idx));
										(e.currentTarget as HTMLElement).style.opacity = "0.4";
									}}
									onDragEnd={(e: React.DragEvent) => { (e.currentTarget as HTMLElement).style.opacity = "1"; }}
									onDragOver={(e: React.DragEvent) => { e.preventDefault(); e.dataTransfer.dropEffect = "move"; }}
									onDrop={(e: React.DragEvent) => {
										e.preventDefault(); e.stopPropagation();
										const fromIdx = parseInt(e.dataTransfer.getData("text/plain"));
										if (isNaN(fromIdx) || fromIdx === idx) return;
										setTimeline((prev: any[]) => {
											const next = [...prev]; const [moved] = next.splice(fromIdx, 1); next.splice(idx, 0, moved); return next;
										});
									}}
									className={`flex-shrink-0 flex items-center gap-2 px-3 py-1.5 rounded text-xs transition-colors border select-none cursor-grab active:cursor-grabbing ${
										idx === activeIdx ? "bg-y/10 border-y/50 text-y" : "bg-muted border-border text-text-mid hover:border-y/30 hover:text-white"
									}`}
								>
									<span className="font-mono truncate max-w-[120px]">{clip.name}</span>
									<span className="text-[9px] opacity-60 tabular-nums">{formatTime(clip.trimOut - clip.trimIn)}</span>
								</div>
							))}
						</div>
						<div onClick={handleBrowse} className={`mt-2 flex items-center justify-center gap-2 py-3 px-4 rounded-lg border-2 border-dashed cursor-pointer transition-colors ${
							dragOver ? "border-y bg-y/10 text-y" : "border-border text-text-dim hover:border-y/50 hover:text-y/80"
						}`}>
							<Plus size={18} />
							<p className="text-xs font-semibold text-center">{dragOver ? "Drop to add to timeline" : "Drag video here or click to browse"}</p>
						</div>
					</div>
				)}

				{/* Video preview */}
				<div className="flex-1 bg-black flex items-center justify-center overflow-hidden min-h-0 p-4 relative">
					<div className="relative bg-black flex items-center justify-center overflow-hidden rounded-lg border border-border w-full h-full">
						{filePath && (
							<video
								key={filePath}
								ref={videoRef}
								src={toFileUrl(filePath)}
								className="max-w-full max-h-full w-auto h-auto"
								style={{ objectFit: "contain" }}
								onLoadedMetadata={onLoadedMetadata}
								onTimeUpdate={onTimeUpdate}
								onEnded={() => setPlaying(false)}
								onError={() => setExportError(`Cannot load file: ${filePath}`)}
							/>
						)}
						<VolumeSync videoRef={videoRef} muted={muted} volume={volume} />
					</div>
				</div>

				{/* Timeline controls */}
				<TimelineControls
					cutMode={cutMode} setCutMode={setCutMode} cutCount={cutCount} setCuts={setCuts}
					duration={duration} ticks={ticks} pct={pct} trimIn={trimIn} trimOut={trimOut}
					cuts={cuts} scissorPos={scissorPos} currentTime={currentTime}
					handleTimelineMouseDown={handleTimelineMouseDown} handleTimelineContext={handleTimelineContext}
					setTrimIn={setTrimIn} setTrimOut={setTrimOut} seek={seek}
					togglePlay={togglePlay} playing={playing} muted={muted} setMuted={setMuted}
					volume={volume} setVol={setVol} timelineScale={timelineScale} setTimelineScale={setTimelineScale}
					videoRef={videoRef} timeline={timeline} activeIdx={activeIdx} selectClip={selectClip}
				/>
			</div>

			{/* Right panel */}
			<RightPanel
				timeline={timeline} activeIdx={activeIdx} filePath={filePath}
				selectClip={selectClip} removeFromTimeline={removeFromTimeline} moveClip={moveClip}
				dragSrcRef={dragSrcRef} setTimeline={setTimeline} setActiveIdx={setActiveIdx}
				trimIn={trimIn} trimOut={trimOut} duration={duration}
				cuts={cuts} setCuts={setCuts} removeCut={removeCut}
				renaming={renaming} setRenaming={setRenaming} renameValue={renameValue} setRenameValue={setRenameValue}
				renameInputRef={renameInputRef} renameRef={renameRef} handleRename={handleRename} cancelRename={cancelRename}
				handleQuickMerge={handleQuickMerge} handleExport={handleExport}
				exporting={exporting} exportDone={exportDone} exportError={exportError} setExportError={setExportError}
				expFormat={expFormat} setExpFormat={setExpFormat}
				expResolution={expResolution} setExpResolution={setExpResolution}
				expAspect={expAspect} setExpAspect={setExpAspect}
				cloud={cloud} uploadJob={uploadJob} uploadBusy={uploadBusy} showUploaded={showUploaded}
				settings={settings}
			/>
		</div>
	);
}


// ── Timeline Controls ───────────────────────────────────────────────────────
function TimelineControls(props: any) {
	const {
		cutMode, setCutMode, cutCount, setCuts,
		duration, ticks, pct, trimIn, trimOut,
		cuts, scissorPos, currentTime,
		handleTimelineMouseDown, handleTimelineContext,
		setTrimIn, setTrimOut, seek,
		togglePlay, playing, muted, setMuted,
		volume, setVol, timelineScale, setTimelineScale,
		videoRef, timeline, activeIdx, selectClip,
	} = props;

	return (
		<div className="bg-[#0d0d0d] border-t border-border px-4 py-3 space-y-2 flex-shrink-0">
			{/* Cut toolbar */}
			<div className="flex items-center gap-2">
				<button
					onClick={() => setCutMode((m: boolean) => !m)}
					className={`flex items-center gap-1.5 px-2.5 py-1 rounded text-xs font-semibold transition-all ${
						cutMode ? "bg-y/20 text-y border border-y/50" : "text-text-mid border border-border hover:border-y/40 hover:text-y/80"
					}`}
					title="Cut mode — click timeline to mark cuts [X]"
				>
					<Scissors size={13} /><span>Cut</span>
				</button>
				{cutCount > 0 && (
					<>
						<span className="text-[10px] text-y font-mono flex items-center gap-1 bg-y/10 px-1.5 py-0.5 rounded">
							{cutCount} cut{cutCount !== 1 ? "s" : ""}
						</span>
						<button onClick={() => setCuts([])} className="text-[10px] text-text-dim hover:text-white transition-colors ml-1">Clear</button>
					</>
				)}
			</div>

			{/* Time ruler */}
			{duration > 0 && (
				<div className="relative h-4 overflow-hidden select-none" style={{ fontSize: 0 }}>
					{ticks.map((t: number) => (
						<div key={t} className="absolute top-0 flex flex-col items-start" style={{ left: `${pct(t)}%`, transform: "translateX(-50%)" }}>
							<div className="h-2 w-px bg-muted" />
							<span className="text-[9px] text-text-dim font-mono mt-0.5" style={{ fontSize: 9 }}>
								{Math.floor(t / 60)}:{String(Math.floor(t % 60)).padStart(2, "0")}
							</span>
						</div>
					))}
				</div>
			)}

			{/* Combined clip track when 2+ clips */}
			{timeline.length > 1 && (
				<div className="flex h-7 rounded overflow-hidden gap-px">
					{(() => {
						const totalDur = timeline.reduce((s: number, e: any) => s + (e.trimOut - e.trimIn), 0);
						const clipColors = ["#D4F00033", "#00B4D833", "#FF6B6B33", "#51CF6633", "#FFA94D33", "#CC99FF33"];
						return timeline.map((clip: any, idx: number) => {
							const clipDur = clip.trimOut - clip.trimIn;
							const p = totalDur > 0 ? (clipDur / totalDur) * 100 : 0;
							const isActive = idx === activeIdx;
							return (
								<div key={clip.id} onClick={() => selectClip(idx)}
									className={`relative flex items-center px-2 text-[9px] font-mono truncate cursor-pointer transition-all ${isActive ? "font-bold ring-1 ring-yellow-400/70 z-10" : "hover:brightness-125"}`}
									style={{ width: `${p}%`, backgroundColor: isActive ? "#D4F00022" : clipColors[idx % clipColors.length] }}
									title={`${clip.name} (${formatTime(clipDur)})`}
								>
									<span className="truncate">{clip.name}</span>
									<span className="ml-auto text-[8px] opacity-70 flex-shrink-0">{formatTime(clipDur)}</span>
								</div>
							);
						});
					})()}
				</div>
			)}

			{/* Main timeline scrubber */}
			<div
				className={`relative h-12 bg-muted rounded-lg overflow-hidden select-none ${cutMode ? "cursor-crosshair" : "cursor-pointer"} group`}
				onMouseDown={handleTimelineMouseDown} onContextMenu={handleTimelineContext}
			>
				{ticks.map((t: number) => (<div key={t} className="absolute top-0 h-full w-px bg-black/30" style={{ left: `${pct(t)}%` }} />))}
				{/* Trim region */}
				<div className="absolute top-0 h-full bg-[#D4F00022]" style={{ left: `${pct(trimIn)}%`, width: `${pct(trimOut) - pct(trimIn)}%` }}>
					<div className="absolute left-0 top-1/2 -translate-y-1/2 -translate-x-1/2 bg-y text-black text-[9px] font-bold px-1.5 py-0.5 rounded shadow-lg pointer-events-none z-30">IN</div>
					<div className="absolute right-0 top-1/2 -translate-y-1/2 translate-x-1/2 bg-y text-black text-[9px] font-bold px-1.5 py-0.5 rounded shadow-lg pointer-events-none z-30">OUT</div>
				</div>
				{/* Cut segments */}
				{cuts.map((c: any, i: number) => (
					<div key={i} className="absolute top-0 h-full bg-red-600/50 z-[5] flex items-center justify-center" style={{ left: `${pct(c.start)}%`, width: `${pct(c.end) - pct(c.start)}%` }}>
						<div className="w-full h-px bg-red-400/60" />
					</div>
				))}
				{cuts.map((c: any, i: number) => {
					const mid = (c.start + c.end) / 2;
					return (<div key={`sc-${i}`} className="absolute top-1/2 -translate-y-1/2 -translate-x-1/2 z-[6]" style={{ left: `${pct(mid)}%` }}><Scissors size={14} className="text-red-300" /></div>);
				})}
				{scissorPos !== null && (
					<div className="absolute top-1/2 -translate-y-1/2 -translate-x-1/2 z-20 pointer-events-none" style={{ left: `${pct(scissorPos)}%` }}>
						<Scissors size={16} className="text-y" />
					</div>
				)}
				{/* Playhead */}
				<div className="absolute top-0 w-0.5 h-full bg-white z-10 shadow-lg" style={{ left: `${pct(currentTime)}%` }}>
					<div className="w-3 h-3 bg-white rounded-full -ml-[5px] -mt-[1px]" />
				</div>
				{/* Trim handles */}
				<TrimHandle position={pct(trimIn)} onDrag={(t: number) => setTrimIn(Math.max(0, Math.min(trimOut - 0.5, t)))} duration={duration} />
				<TrimHandle position={pct(trimOut)} onDrag={(t: number) => setTrimOut(Math.min(duration, Math.max(trimIn + 0.5, t)))} duration={duration} />
			</div>

			{/* Time display */}
			<div className="flex items-center justify-between gap-4">
				<div className="flex items-center gap-3 text-xs text-text-dim font-mono">
					<span className="flex items-center gap-1"><span className="text-y font-bold text-[10px]">IN</span>{formatTime(trimIn)}</span>
					<span className="text-white font-bold">{formatTime(currentTime)}</span>
					<span className="flex items-center gap-1"><span className="text-y font-bold text-[10px]">OUT</span>{formatTime(trimOut)}</span>
					<span className="text-text-mid text-[10px]">({formatTime(trimOut - trimIn)})</span>
				</div>
				<div className="flex items-center gap-2">
					<span className="text-[10px] text-text-dim">Zoom</span>
					<input type="range" min={1} max={5} step={0.5} value={timelineScale} onChange={(e) => setTimelineScale(Number(e.target.value))} className="w-16 accent-[#D4F000] no-drag" />
				</div>
			</div>

			{/* Playback controls */}
			<div className="flex items-center justify-between">
				<div className="flex items-center gap-2">
					<button onClick={() => seek(trimIn)} className="text-text-mid hover:text-white transition-colors p-1" title="Jump to IN"><SkipBack size={16} /></button>
					<button onClick={togglePlay} className="w-8 h-8 rounded-full bg-y hover:bg-yd flex items-center justify-center transition-colors">
						{playing ? <Pause size={15} fill="black" className="text-black" /> : <Play size={15} fill="black" className="text-black" />}
					</button>
					<button onClick={() => seek(trimOut)} className="text-text-mid hover:text-white transition-colors p-1" title="Jump to OUT"><SkipForward size={16} /></button>
					<div className="w-px h-5 bg-border mx-2" />
					<button onClick={() => { const t = videoRef.current?.currentTime ?? 0; setTrimIn(t); if (videoRef.current) videoRef.current.currentTime = t; }} className="text-[10px] font-bold px-2 py-1 rounded border border-border text-text-mid hover:border-y hover:text-y transition-colors" title="Set IN [I]">Set IN</button>
					<button onClick={() => { const t = videoRef.current?.currentTime ?? 0; setTrimOut(t); }} className="text-[10px] font-bold px-2 py-1 rounded border border-border text-text-mid hover:border-y hover:text-y transition-colors" title="Set OUT [O]">Set OUT</button>
					<button onClick={() => setMuted((m: boolean) => !m)} className="text-text-mid hover:text-white transition-colors p-1">
						{muted ? <VolumeX size={14} /> : <Volume2 size={14} />}
					</button>
					<input type="range" min={0} max={1} step={0.05} value={muted ? 0 : volume} onChange={(e) => setVol(Number(e.target.value))} className="w-16 accent-[#D4F000] no-drag" />
					<div className="w-px h-5 bg-border mx-2" />
					<button onClick={() => { setTrimIn(0); setTrimOut(duration); }} className="text-text-dim hover:text-white transition-colors p-1" title="Reset trim"><RotateCcw size={14} /></button>
				</div>
				<div className="text-[9px] text-text-dim hidden md:block">
					<span className="inline-flex items-center gap-2">
						<span><kbd className="bg-muted px-1 rounded">I</kbd> IN</span>
						<span><kbd className="bg-muted px-1 rounded">O</kbd> OUT</span>
						<span><kbd className="bg-muted px-1 rounded">X</kbd> Cut</span>
						<span><kbd className="bg-muted px-1 rounded">Space</kbd> Play</span>
					</span>
				</div>
			</div>
		</div>
	);
}


// ── Right Panel ─────────────────────────────────────────────────────────────
function RightPanel(props: any) {
	const {
		timeline, activeIdx, filePath,
		selectClip, removeFromTimeline, moveClip,
		dragSrcRef, setTimeline, setActiveIdx,
		trimIn, trimOut, duration,
		cuts, setCuts, removeCut,
		renaming, setRenaming, renameValue, setRenameValue,
		renameInputRef, renameRef, handleRename, cancelRename,
		handleQuickMerge, handleExport,
		exporting, exportDone, exportError, setExportError,
		expFormat, setExpFormat,
		expResolution, setExpResolution,
		expAspect, setExpAspect,
		cloud, uploadJob, uploadBusy, showUploaded,
		settings,
	} = props;

	return (
		<div className="w-72 flex-shrink-0 border-l border-border bg-[#0d0d0d] flex flex-col overflow-y-auto p-4 space-y-4">
			{/* Timeline clips list */}
			<div className="space-y-2">
				<span className="text-[10px] text-text-dim">{timeline.length} clip{timeline.length !== 1 ? "s" : ""}</span>
				{timeline.length === 0 ? (
					<p className="text-text-dim text-xs">Drop or open a video to start.</p>
				) : (
					<div className="space-y-1 max-h-48 overflow-y-auto">
						{timeline.map((clip: any, idx: number) => (
							<div key={clip.id}
								draggable
								onDragStart={() => { dragSrcRef.current = idx; }}
								onDragOver={(e: React.DragEvent) => { e.preventDefault(); e.dataTransfer.dropEffect = "move"; }}
								onDragEnter={(e: React.DragEvent) => {
									e.preventDefault();
									if (dragSrcRef.current !== null && dragSrcRef.current !== idx) {
										(e.currentTarget as HTMLElement).style.borderTopColor = "#D4F000";
										(e.currentTarget as HTMLElement).style.borderTopWidth = "2px";
									}
								}}
								onDragLeave={(e: React.DragEvent) => { (e.currentTarget as HTMLElement).style.borderTopColor = ""; (e.currentTarget as HTMLElement).style.borderTopWidth = ""; }}
								onDrop={(e: React.DragEvent) => {
									e.preventDefault();
									(e.currentTarget as HTMLElement).style.borderTopColor = "";
									(e.currentTarget as HTMLElement).style.borderTopWidth = "";
									const from = dragSrcRef.current;
									dragSrcRef.current = null;
									if (from !== null && from !== idx) {
										setTimeline((prev: any[]) => { const next = [...prev]; const [moved] = next.splice(from!, 1); next.splice(idx, 0, moved); return next; });
										setActiveIdx(idx);
									}
								}}
								onDragEnd={() => { dragSrcRef.current = null; }}
								className={`flex items-center gap-1 rounded px-2 py-1.5 text-xs cursor-pointer transition-colors ${
									idx === activeIdx ? "bg-y/10 border border-y/40" : "bg-muted border border-transparent hover:border-border"
								}`}
								onClick={() => selectClip(idx)}
							>
								<GripHorizontal size={11} className="text-text-dim flex-shrink-0 opacity-40 cursor-grab" />
								<span className="truncate flex-1 text-text-mid font-mono" title={clip.name}>{clip.name}</span>
								<span className="text-text-dim text-[9px] font-mono tabular-nums flex-shrink-0">{formatTime(clip.trimOut - clip.trimIn)}</span>
								<button onClick={(e: React.MouseEvent) => { e.stopPropagation(); moveClip(idx, -1); }} disabled={idx === 0} className="p-0.5 text-text-dim hover:text-white disabled:opacity-20 flex-shrink-0"><ArrowUp size={10} /></button>
								<button onClick={(e: React.MouseEvent) => { e.stopPropagation(); moveClip(idx, 1); }} disabled={idx === timeline.length - 1} className="p-0.5 text-text-dim hover:text-white disabled:opacity-20 flex-shrink-0"><ArrowDown size={10} /></button>
								<button onClick={(e: React.MouseEvent) => { e.stopPropagation(); removeFromTimeline(idx); }} className="p-0.5 text-red-400 hover:text-red-300 flex-shrink-0"><Trash2 size={10} /></button>
							</div>
						))}
					</div>
				)}
			</div>

			<hr className="border-border" />

			{/* Active clip name */}
			{filePath && (
				<div className="space-y-1">
					<p className="label">Active Clip</p>
					{renaming ? (
						<div ref={renameRef} className="flex items-center gap-1">
							<input ref={renameInputRef} value={renameValue} onChange={(e) => setRenameValue(e.target.value)}
								onKeyDown={(e: React.KeyboardEvent) => { if (e.key === "Enter") handleRename(); if (e.key === "Escape") cancelRename(); }}
								className="bg-[#1a1a1a] border border-border rounded-lg px-2 py-1 text-xs text-white flex-1 min-w-0 outline-none focus:border-y transition-colors" autoFocus />
							<button onMouseDown={(e: React.MouseEvent) => e.preventDefault()} onClick={handleRename} className="p-1.5 rounded bg-y text-black hover:bg-yd transition-colors"><Check size={13} /></button>
						</div>
					) : (
						<div className="flex items-center gap-1 group">
							<span className="text-xs text-white truncate flex-1 font-mono">{timeline[activeIdx]?.name ?? filePath.replace(/^.*[\\/]/, "")}</span>
							<button onClick={() => { const entry = timeline[activeIdx]; const name = (entry?.name ?? "").replace(/\.[^.]+$/, ""); setRenameValue(name); setRenaming(true); setTimeout(() => renameInputRef.current?.select(), 50); }}
								className="p-1 text-text-dim hover:text-y border border-transparent hover:border-y rounded transition-colors"><Pen size={11} /></button>
						</div>
					)}
				</div>
			)}

			{/* Merge button */}
			{timeline.length > 1 && (
				<button onClick={handleQuickMerge} disabled={exporting}
					className="w-full flex items-center justify-center gap-2 py-2.5 rounded-lg border-2 border-y/40 bg-y/5 text-y text-xs font-bold hover:bg-y/10 transition-colors disabled:opacity-50">
					{exporting ? <><Loader2 size={14} className="animate-spin" /> Merging…</> : <><Scissors size={14} /> Merge {timeline.length} Clips</>}
				</button>
			)}

			{/* Trim info */}
			<Section title="Trim">
				<Row label="In" val={formatTime(trimIn)} />
				<Row label="Out" val={formatTime(trimOut)} />
				<Row label="Duration" val={formatTime(trimOut - trimIn)} />
			</Section>

			{/* Cuts */}
			{timeline.length === 1 && (
				<Section title="Cuts / Removed Segments">
					{cuts.length === 0 ? (
						<p className="text-text-dim text-xs">Right-click the timeline or press <kbd className="bg-muted px-1 rounded">X</kbd> to cut.</p>
					) : (
						<div className="space-y-1 max-h-40 overflow-y-auto">
							{cuts.map((c: any, i: number) => (
								<div key={i} className="flex items-center justify-between bg-muted rounded px-2 py-1 text-xs">
									<span className="text-text-mid font-mono">{formatTime(c.start)} – {formatTime(c.end)} <span className="text-text-dim ml-1">({formatTime(c.end - c.start)})</span></span>
									<button onClick={() => removeCut(i)} className="text-red-400 hover:text-red-300 flex-shrink-0 ml-1"><Trash2 size={12} /></button>
								</div>
							))}
						</div>
					)}
					{cuts.length > 0 && (
						<button onClick={() => setCuts([])} className="text-[10px] text-text-dim hover:text-white transition-colors flex items-center gap-1 mt-1"><RotateCcw size={10} /> Clear all cuts</button>
					)}
				</Section>
			)}

			{/* Export Presets */}
			<Section title="Export Presets">
				<div className="grid grid-cols-3 gap-1.5">
					<button onClick={() => { setExpFormat("mp4"); setExpResolution("1080p"); setExpAspect("16:9"); }} className="text-[10px] font-medium px-2 py-1.5 rounded border border-border hover:border-y hover:text-y transition-colors text-center">YouTube</button>
					<button onClick={() => { setExpFormat("mp4"); setExpResolution("1080p"); setExpAspect("9:16"); }} className="text-[10px] font-medium px-2 py-1.5 rounded border border-border hover:border-y hover:text-y transition-colors text-center">TikTok</button>
					<button onClick={() => { setExpFormat("mp4"); setExpResolution("720p"); setExpAspect("16:9"); }} className="text-[10px] font-medium px-2 py-1.5 rounded border border-border hover:border-y hover:text-y transition-colors text-center">Twitter</button>
					<button onClick={() => { setExpFormat("mp4"); setExpResolution("720p"); setExpAspect("16:9"); }} className="text-[10px] font-medium px-2 py-1.5 rounded border border-border hover:border-y hover:text-y transition-colors text-center">Discord</button>
					<button onClick={() => { setExpFormat("mp4"); setExpResolution("1080p"); setExpAspect("9:16"); }} className="text-[10px] font-medium px-2 py-1.5 rounded border border-border hover:border-y hover:text-y transition-colors text-center">Reels</button>
					<button onClick={() => { setExpFormat("mp4"); setExpResolution("1440p"); setExpAspect("16:9"); }} className="text-[10px] font-medium px-2 py-1.5 rounded border border-border hover:border-y hover:text-y transition-colors text-center">Max Quality</button>
				</div>
			</Section>

			<Section title="Format">
				<Select label="Container" value={expFormat} onChange={setExpFormat} options={["mp4", "webm", "mkv", "mov"]} />
			</Section>

			<Section title="Video">
				<Select label="Resolution" value={expResolution} onChange={setExpResolution} options={["480p", "720p", "1080p", "1440p", "4k"]} />
				<Select label="Aspect Ratio" value={expAspect} onChange={setExpAspect} options={["16:9", "9:16", "4:3", "21:9"]} />
			</Section>

			<AspectPreview ratio={expAspect} />

			{exportDone && (
				<div className="bg-[#0a1a00] border border-[#2a4a00] rounded-lg p-3 text-xs text-[#aaff44]">
					✓ Exported:<br /><span className="text-text-mid break-all">{exportDone.split(/[\\/]/).pop()}</span>
					<button className="mt-1 text-y underline block" onClick={() => bridge.showInFolder(exportDone)}>Show in folder</button>
				</div>
			)}
			{exportError && (
				<div className="bg-red-900/30 border border-red-700 rounded-lg p-3 text-xs text-red-300 flex items-start gap-2">
					<span className="flex-1">⚠ {exportError}</span>
					<button onClick={() => setExportError(null)} className="text-red-400 hover:text-white flex-shrink-0 mt-0.5">✕</button>
				</div>
			)}

			{/* Cloud upload */}
			{cloud.paired && filePath && (
				<>
					<button
						onClick={async () => {
							if (uploadBusy) return;
							if (uploadJob?.status === "failed") { cloud.retryJob(uploadJob.id); return; }
							const entry = timeline[activeIdx];
							const name = entry?.name ?? filePath.replace(/^.*[\\/]/, "");
							const stat = await bridge.getFileStats(filePath).catch(() => null);
							cloud.addToQueue(filePath, name, stat?.size ?? 0, {
								trimStart: trimIn > 0 ? trimIn : undefined,
								trimEnd: trimOut < duration ? trimOut : undefined,
								cuts: cuts.length > 0 ? cuts : undefined,
							});
						}}
						disabled={uploadBusy}
						className={`btn-ghost justify-center w-full py-2 ${uploadBusy ? "opacity-50" : ""} ${showUploaded ? "text-green-400" : ""} ${uploadJob?.status === "failed" ? "text-red-400" : ""}`}
					>
						{uploadBusy && <><Loader2 size={14} className="animate-spin" /> {uploadJob!.status === "queued" ? "Queued…" : "Uploading…"}</>}
						{showUploaded && !uploadBusy && <><Upload size={14} /> Uploaded</>}
						{!uploadBusy && !showUploaded && <><Upload size={14} /> Upload to Cloud</>}
					</button>
					{uploadJob?.status === "failed" && uploadJob?.error && (
						<div className="bg-red-900/30 border border-red-700 rounded-lg p-3 text-xs text-red-300 mt-1 flex items-start gap-2">
							<span className="flex-1">Upload failed: {uploadJob.error}</span>
							<button onClick={() => cloud.removeJob(uploadJob.id)} className="text-red-400 hover:text-white flex-shrink-0 mt-0.5">✕</button>
						</div>
					)}
				</>
			)}

			<button onClick={handleExport} disabled={exporting} className="btn-y justify-center w-full py-3 disabled:opacity-50">
				{exporting ? <><Loader2 size={16} className="animate-spin" /> Exporting…</> : <><Download size={16} /> Export Clip</>}
			</button>
		</div>
	);
}


// ── Helper Components ───────────────────────────────────────────────────────
function VolumeSync({ videoRef, muted, volume }: { videoRef: React.RefObject<HTMLVideoElement | null>; muted: boolean; volume: number }) {
	useEffect(() => {
		const v = videoRef.current;
		if (v) { v.muted = muted; v.volume = volume; }
	}, [muted, volume, videoRef]);
	return null;
}

function TrimHandle({ position, onDrag, duration }: { position: number; onDrag: (t: number) => void; duration: number }) {
	return (
		<div
			className="absolute top-0 h-full w-6 -translate-x-1/2 flex items-center cursor-ew-resize group/handle z-20"
			style={{ left: `${position}%` }}
			onMouseDown={(e) => {
				e.stopPropagation();
				const parent = e.currentTarget.parentElement!;
				const move = (me: MouseEvent) => {
					const rect = parent.getBoundingClientRect();
					const t = ((me.clientX - rect.left) / rect.width) * duration;
					onDrag(t);
				};
				const up = () => { window.removeEventListener("mousemove", move); window.removeEventListener("mouseup", up); };
				window.addEventListener("mousemove", move);
				window.addEventListener("mouseup", up);
			}}
		>
			<div className="w-2 h-full bg-y group-hover/handle:shadow-[0_0_12px_#D4F000]" />
		</div>
	);
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
	return (
		<div className="space-y-2">
			<p className="label">{title}</p>
			<div className="space-y-2">{children}</div>
		</div>
	);
}

function Select({ label, value, onChange, options }: { label: string; value: string; onChange: (v: string) => void; options: string[] }) {
	return (
		<div className="space-y-1">
			<p className="text-text-dim text-[10px]">{label}</p>
			<select value={value} onChange={(e) => onChange(e.target.value)} className="input text-xs py-1.5 no-drag">
				{options.map((o) => <option key={o} value={o}>{o}</option>)}
			</select>
		</div>
	);
}

function Row({ label, val }: { label: string; val: string }) {
	return (
		<div className="flex justify-between text-xs">
			<span className="text-text-dim">{label}</span>
			<span className="text-white font-mono">{val}</span>
		</div>
	);
}

function AspectPreview({ ratio }: { ratio: string }) {
	const map: Record<string, string> = { "16:9": "w-20 h-[45px]", "9:16": "w-10 h-[71px]", "4:3": "w-16 h-12", "21:9": "w-24 h-[41px]" };
	const cls = map[ratio] ?? "w-16 h-9";
	return (
		<div className="flex flex-col items-center gap-1">
			<p className="label">Preview</p>
			<div className={`${cls} bg-muted border border-y rounded flex items-center justify-center`}><Crop size={14} className="text-y" /></div>
			<p className="text-text-dim text-xs">{ratio}</p>
		</div>
	);
}
