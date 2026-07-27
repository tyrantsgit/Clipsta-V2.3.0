import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Film, FolderOpen, Trash2, Scissors, Play, RefreshCw, Search, Upload, Download, Loader2, CheckCircle, XCircle, Plus } from "lucide-react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { ClipFile, UploadJob } from "../../types";
import type { useCloudUpload } from "../../hooks/useCloudUpload";
import bridge from "../../tauri-bridge";

function toFileUrl(p: string): string {
	// Use Tauri's asset protocol for secure file access in the webview
	return convertFileSrc(p);
}

function formatSize(bytes: number) {
	if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
	return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export default function LibraryPage({ onOpenEditor, cloud }: { onOpenEditor: (path: string) => void; cloud: ReturnType<typeof useCloudUpload> }) {
	const [clips, setClips] = useState<ClipFile[]>([]);
	const [loading, setLoading] = useState(true);
	const [selected, setSelected] = useState<ClipFile | null>(null);
	const [search, setSearch] = useState("");
	const [playing, setPlaying] = useState<string | null>(null);
	const [dragOver, setDragOver] = useState(false);
	const dragCounterRef = useRef(0);
	const videoRef = useRef<HTMLVideoElement>(null);

	const load = useCallback(async () => {
		setLoading(true);
		try {
			const list = await bridge.listClips();
			setClips(list);
			setSelected((prev) => (prev && list.some((c) => c.path === prev.path) ? prev : null));
		} catch { setClips([]); }
		finally { setLoading(false); }
	}, []);

	useEffect(() => { load(); }, [load]);

	// Tauri 2 native drag-drop: provides full file paths from OS
	const loadRef = useRef(load);
	loadRef.current = load;
	const [dropAnimation, setDropAnimation] = useState(false);
	useEffect(() => {
		const unlisten = getCurrentWindow().onDragDropEvent(async (event) => {
			if (event.payload.type === "enter" || event.payload.type === "over") {
				setDragOver(true);
			} else if (event.payload.type === "leave") {
				setDragOver(false);
			} else if (event.payload.type === "drop") {
				setDragOver(false);
				const paths = (event.payload.paths ?? []).filter((p: string) =>
					/\.(webm|mp4|mkv|mov)$/i.test(p)
				);
				if (paths.length === 0) return;
				// Show success drop animation
				setDropAnimation(true);
				setTimeout(() => setDropAnimation(false), 800);

				const imported: string[] = [];
				for (const p of paths) {
					try { const dest = await bridge.importClip(p); if (dest) imported.push(dest); } catch { /* skip */ }
				}
				if (cloud.paired) {
					for (const p of imported) {
						const name = p.replace(/^.*[\\/]/, "");
						const stat = await bridge.getFileStats(p).catch(() => null);
						cloud.addToQueue(p, name, stat?.size ?? 0);
					}
				}
				await loadRef.current();
				// Auto-select and preview the first imported clip
				if (imported.length > 0) {
					const newClip: ClipFile = {
						name: imported[0].replace(/^.*[\\/]/, ""),
						path: imported[0],
						size: 0,
						createdAt: new Date().toISOString(),
					};
					setSelected(newClip);
					setPlaying(imported[0]);
					setTimeout(() => videoRef.current?.play().catch(() => {}), 200);
				}
			}
		});
		return () => { unlisten.then((u) => u()); };
	}, [cloud]);

	const addClip = async () => {
		const path = await bridge.browseFile();
		if (!path) return;
		const name = path.replace(/^.*[\\/]/, "");
		if (!confirm(`Add "${name}" to the library?`)) return;
		const destPath = await bridge.importClip(path);
		if (destPath && cloud.paired) {
			const stat = await bridge.getFileStats(destPath).catch(() => null);
			cloud.addToQueue(destPath, name, stat?.size ?? 0);
		}
		load();
	};

	const addFolder = async () => {
		const folder = await bridge.browseImportFolder();
		if (!folder) return;
		const imported = await bridge.importFolder(folder);
		if (imported.length === 0) { alert("No video files found in the selected folder."); return; }
		const names = imported.map((p) => p.replace(/^.*[\\/]/, ""));
		if (!confirm(`Add ${imported.length} clip${imported.length !== 1 ? "s" : ""}?\n${names.join("\n")}`)) return;
		if (cloud.paired) {
			for (const p of imported) {
				const name = p.replace(/^.*[\\/]/, "");
				const stat = await bridge.getFileStats(p).catch(() => null);
				cloud.addToQueue(p, name, stat?.size ?? 0);
			}
		}
		load();
	};

	const handleDragEnter = (e: React.DragEvent) => { e.preventDefault(); dragCounterRef.current++; setDragOver(true); };
	const handleDragOver = (e: React.DragEvent) => { e.preventDefault(); };
	const handleDragLeave = (e: React.DragEvent) => { e.preventDefault(); dragCounterRef.current--; if (dragCounterRef.current <= 0) { dragCounterRef.current = 0; setDragOver(false); } };

	const handleDrop = async (e: React.DragEvent) => {
		e.preventDefault(); dragCounterRef.current = 0; setDragOver(false);
		const paths: string[] = [];
		for (const f of Array.from(e.dataTransfer.files)) {
			const p = (f as any).path || f.name;
			if (p && /\.(webm|mp4|mkv|mov)$/i.test(p)) paths.push(p);
		}
		if (paths.length === 0) return;
		setDropAnimation(true);
		setTimeout(() => setDropAnimation(false), 800);
		const imported: string[] = [];
		for (const p of paths) {
			try { const dest = await bridge.importClip(p); if (dest) imported.push(dest); } catch { /* skip */ }
		}
		if (cloud.paired) {
			for (const p of imported) {
				const name = p.replace(/^.*[\\/]/, "");
				const stat = await bridge.getFileStats(p).catch(() => null);
				cloud.addToQueue(p, name, stat?.size ?? 0);
			}
		}
		await load();
		// Auto-select and preview the first imported clip
		if (imported.length > 0) {
			const newClip: ClipFile = {
				name: imported[0].replace(/^.*[\\/]/, ""),
				path: imported[0],
				size: 0,
				createdAt: new Date().toISOString(),
			};
			setSelected(newClip);
			setPlaying(imported[0]);
			setTimeout(() => videoRef.current?.play().catch(() => {}), 200);
		}
	};

	const deleteClip = async (clip: ClipFile) => {
		const { ask } = await import("@tauri-apps/plugin-dialog");
		const confirmed = await ask(`Are you sure you want to delete "${clip.name}"?\n\nThis cannot be undone.`, {
			title: "Delete Clip",
			kind: "warning",
		});
		if (!confirmed) return;
		try {
			if (videoRef.current) { videoRef.current.pause(); videoRef.current.removeAttribute("src"); videoRef.current.load(); }
			setPlaying(null); setSelected(null);
			await new Promise((r) => setTimeout(r, 100));
			await bridge.deleteClip(clip.path);
			load();
		} catch (e) { console.error("Delete failed:", e); alert("Failed to delete clip."); }
	};

	const playClip = (clip: ClipFile) => {
		setSelected(clip); setPlaying(clip.path);
		setTimeout(() => videoRef.current?.play().catch(() => {}), 100);
	};

	const downloadClip = async (clip: ClipFile) => {
		try {
			const dest = await bridge.copyToDownloads(clip.path);
			if (dest) { const name = dest.split(/[\\/]/).pop() ?? dest; alert(`Downloaded:\n${name}`); }
		} catch { alert("Failed to download clip."); }
	};

	const filtered = useMemo(() => {
		const lowered = search.toLowerCase();
		return clips.filter((c) => c.name.toLowerCase().includes(lowered));
	}, [clips, search]);

	const getUploadStatus = (clip: ClipFile): UploadJob | undefined => cloud.queue.find((j) => j.path === clip.path);

	return (
		<div className="h-full flex overflow-hidden" onDragEnter={handleDragEnter} onDragOver={handleDragOver} onDragLeave={handleDragLeave} onDrop={handleDrop}>
			{(dragOver || dropAnimation) && (
				<div className={`absolute inset-0 z-50 flex items-center justify-center pointer-events-none transition-all duration-300 ${dragOver ? "bg-black/70" : "bg-black/40"}`}>
					<div className={`rounded-2xl border-2 border-dashed p-10 text-center backdrop-blur-sm transition-all duration-300 ${
						dropAnimation
							? "border-green-400 bg-green-900/30 scale-110"
							: "border-y bg-[#1c1c00]/80 scale-100 animate-pulse"
					}`}>
						<Film size={40} className={`mx-auto mb-3 transition-all duration-300 ${
							dropAnimation ? "text-green-400 animate-bounce" : "text-y animate-bounce"
						}`} />
						<p className={`text-lg font-bold transition-colors ${dropAnimation ? "text-green-400" : "text-y"}`}>
							{dropAnimation ? "✓ Added to library!" : "Drop to add to library"}
						</p>
						<p className="text-text-dim text-sm mt-1">
							{dropAnimation ? "Playing preview…" : "Supports MP4, WebM, MKV, MOV"}
						</p>
					</div>
				</div>
			)}
			{/* Sidebar list */}
			<div className="w-72 flex-shrink-0 border-r border-border flex flex-col">
				<div className="p-4 border-b border-border space-y-3">
					<div className="flex items-center justify-between">
						<h2 className="font-bold text-white">Library</h2>
						<div className="flex items-center gap-2">
							<button onClick={load} className="text-text-dim hover:text-y transition-colors" title="Refresh"><RefreshCw size={14} className={loading ? "animate-spin" : ""} /></button>
							<button onClick={addClip} className="text-text-dim hover:text-y transition-colors" title="Add file"><Plus size={14} /></button>
							<button onClick={addFolder} className="text-text-dim hover:text-y transition-colors" title="Add folder"><FolderOpen size={14} /></button>
						</div>
					</div>
					<div className="relative">
						<Search size={13} className="absolute left-3 top-1/2 -translate-y-1/2 text-text-dim" />
						<input value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Search clips…" className="input pl-8" />
					</div>
					<p className="text-text-dim text-xs">{filtered.length} clip{filtered.length !== 1 ? "s" : ""}</p>
				</div>
				<div className="flex-1 overflow-y-auto p-2 space-y-1">
					{loading && <div className="text-center py-10 text-text-dim text-sm">Loading…</div>}
					{!loading && filtered.length === 0 && (
						<div className="text-center py-10">
							<Film size={36} className="mx-auto mb-3 text-text-dim opacity-40" />
							<p className="text-text-dim text-sm">No clips found</p>
						</div>
					)}
					{filtered.map((clip) => (
						<ClipRow key={clip.path} clip={clip} active={selected?.path === clip.path}
							onClick={() => setSelected(clip)} onPlay={() => playClip(clip)}
							onDelete={() => deleteClip(clip)} onEdit={() => onOpenEditor(clip.path)}
							onUpload={() => cloud.addToQueue(clip.path, clip.name, clip.size)}
							uploadStatus={cloud.queue.find((j) => j.path === clip.path)} />
					))}
				</div>
			</div>

			{/* Viewer */}
			<div className="flex-1 flex flex-col overflow-hidden">
				{selected ? (
					<>
						<div className="flex-1 bg-black flex items-center justify-center overflow-hidden">
							<video ref={videoRef} src={playing === selected.path ? toFileUrl(selected.path) : undefined}
								className="max-w-full max-h-full" controls onEnded={() => setPlaying(null)} />
						</div>
						<div className="p-4 border-t border-border flex items-center gap-3">
							<div className="flex-1 min-w-0">
								<p className="text-white font-semibold text-sm truncate">{selected.name}</p>
								<p className="text-text-dim text-xs">{formatSize(selected.size)} · {new Date(selected.createdAt).toLocaleString()}</p>
							</div>
							<div className="flex gap-2">
								{cloud.paired && (() => {
									const status = getUploadStatus(selected);
									if (status?.status === "done") return <span className="text-green-400 text-[10px] flex items-center gap-1"><CheckCircle size={12} /> Uploaded</span>;
									if (status?.status === "uploading") return <span className="text-y text-[10px] flex items-center gap-1"><Loader2 size={12} className="animate-spin" /> {status.progress}%</span>;
									if (status?.status === "queued") return <span className="text-text-dim text-[10px] flex items-center gap-1"><Loader2 size={12} className="animate-spin" /> Queued</span>;
									if (status?.status === "failed") return (<><span className="text-red-400 text-[10px] flex items-center gap-1"><XCircle size={12} /></span><button onClick={() => cloud.retryJob(status.id)} className="btn-ghost text-[10px]">Retry</button></>);
									return <button onClick={() => cloud.addToQueue(selected.path, selected.name, selected.size)} className="btn-ghost text-[10px]"><Upload size={12} /> Upload</button>;
								})()}
								<button onClick={() => playClip(selected)} className="btn-ghost"><Play size={14} /> Play</button>
								<button onClick={() => onOpenEditor(selected.path)} className="btn-ghost"><Scissors size={14} /> Edit</button>
								<button onClick={() => bridge.showInFolder(selected.path)} className="btn-ghost"><FolderOpen size={14} /> Show</button>
								<button onClick={() => downloadClip(selected)} className="btn-ghost"><Download size={14} /> Download</button>
								<button onClick={() => deleteClip(selected)} className="btn-danger"><Trash2 size={14} /></button>
							</div>
						</div>
					</>
				) : (
					<div className="flex-1 flex items-center justify-center text-center">
						<div>
							<Film size={64} className="mx-auto mb-4 text-text-dim opacity-20" />
							<p className="text-text-mid text-lg font-semibold">Select a clip to preview</p>
							<p className="text-text-dim text-sm mt-1">Your recorded clips appear in the list</p>
						</div>
					</div>
				)}
			</div>
		</div>
	);
}


// ── Thumbnail cache ─────────────────────────────────────────────────────────
const MAX_THUMB_CACHE = 50;
const thumbCache = new Map<string, string>();
const thumbQueue = new Set<string>();

function useThumbnail(path: string): string | null {
	const [thumb, setThumb] = useState<string | null>(() => thumbCache.get(path) ?? null);

	useEffect(() => {
		if (!path) return;
		if (thumbCache.has(path)) { setThumb(thumbCache.get(path)!); return; }
		if (thumbQueue.has(path)) return;
		thumbQueue.add(path);
		let cancelled = false;
		const video = document.createElement("video");
		video.crossOrigin = "anonymous";
		video.muted = true;
		video.preload = "metadata";
		video.src = toFileUrl(path);
		video.onloadeddata = () => { if (!cancelled) video.currentTime = 1; };
		video.onseeked = () => {
			if (cancelled) return;
			try {
				const canvas = document.createElement("canvas");
				canvas.width = 160; canvas.height = 90;
				const ctx = canvas.getContext("2d")!;
				ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
				const dataUrl = canvas.toDataURL("image/jpeg", 0.6);
				if (thumbCache.size >= MAX_THUMB_CACHE) { const firstKey = thumbCache.keys().next().value; if (firstKey) thumbCache.delete(firstKey); }
				thumbCache.set(path, dataUrl);
				thumbQueue.delete(path);
				setThumb(dataUrl);
			} catch { thumbQueue.delete(path); }
			video.remove();
		};
		video.onerror = () => { thumbQueue.delete(path); video.remove(); };
		return () => { cancelled = true; video.remove(); };
	}, [path]);

	return thumb;
}

// ── ClipRow ─────────────────────────────────────────────────────────────────
function ClipRow({ clip, active, onClick, onPlay, onDelete, onEdit, onUpload, uploadStatus }: {
	clip: ClipFile; active: boolean;
	onClick: () => void; onPlay: () => void; onDelete: () => void; onEdit: () => void;
	onUpload?: () => void; uploadStatus?: UploadJob;
}) {
	const thumb = useThumbnail(clip.path);

	return (
		<div onClick={onClick} onDoubleClick={onPlay}
			className={`rounded-lg p-2.5 cursor-pointer border transition-all group ${active ? "border-y bg-[#1c1c00]" : "border-transparent hover:border-border hover:bg-card"}`}>
			<div className="flex items-center gap-2">
				<div className="w-14 h-9 bg-muted rounded flex-shrink-0 overflow-hidden flex items-center justify-center">
					{thumb ? <img src={thumb} alt="" className="w-full h-full object-cover" /> : <Film size={16} className="text-text-dim" />}
				</div>
				<div className="flex-1 min-w-0">
					<p className={`text-xs font-semibold truncate ${active ? "text-y" : "text-white"}`}>{clip.name.replace(/\.[^.]+$/, "")}</p>
					<p className="text-[10px] text-text-dim">{formatSize(clip.size)} · {new Date(clip.createdAt).toLocaleDateString()}</p>
				</div>
				<div className="flex items-center gap-1">
					<button onClick={(e) => { e.stopPropagation(); onEdit(); }} className="p-1 text-text-dim hover:text-y border border-transparent hover:border-y rounded transition-colors" title="Edit"><Scissors size={11} /></button>
				</div>
				<div className="hidden group-hover:flex items-center gap-1">
					<button onClick={(e) => { e.stopPropagation(); onPlay(); }} className="p-1 hover:text-y text-text-dim transition-colors"><Play size={11} /></button>
					<button onClick={(e) => { e.stopPropagation(); onDelete(); }} className="p-1 hover:text-red-400 text-text-dim transition-colors"><Trash2 size={11} /></button>
					{onUpload && !uploadStatus && <button onClick={(e) => { e.stopPropagation(); onUpload(); }} className="p-1 hover:text-y text-text-dim transition-colors" title="Upload"><Upload size={11} /></button>}
					{uploadStatus?.status === "queued" && <Loader2 size={11} className="text-text-dim animate-spin" />}
					{uploadStatus?.status === "uploading" && <Loader2 size={11} className="text-y animate-spin" />}
					{uploadStatus?.status === "done" && <CheckCircle size={11} className="text-green-400" />}
					{uploadStatus?.status === "failed" && <button onClick={(e) => { e.stopPropagation(); onUpload?.(); }} className="p-1 hover:text-y text-text-dim transition-colors"><Upload size={11} className="text-red-400" /></button>}
				</div>
			</div>
		</div>
	);
}
