import type { AppSettings } from "../types";
import type { RecorderState } from "../hooks/useRecorder";

export default function StatusBar({ recorder, settings }: {
	recorder: { state: RecorderState };
	settings: AppSettings;
}) {
	const { status, duration } = recorder.state;
	return (
		<div className="h-7 bg-[#080808] border-t border-border flex items-center px-4 gap-6 text-[11px] text-text-dim flex-shrink-0">
			<span className={status === "recording" ? "text-green-400 font-semibold" : ""}>
				{status === "recording" ? `⏺ Buffer ${formatDur(duration)}` : status === "saving" ? "💾 Saving..." : "⏳ Starting..."}
			</span>
			<span>
				<span className="text-text-mid">{settings.hotkeyClip30Sec || "Win+Alt+G"}</span> 30s ·{" "}
				<span className="text-text-mid">{settings.hotkeyClip1Min}</span> 1min ·{" "}
				<span className="text-text-mid">{settings.hotkeyClip5Min}</span> 5min
			</span>
			<span className="ml-auto">{settings.resolution} · {settings.fps}fps · {settings.bitrate >= 1000 ? `${Math.round(settings.bitrate / 1000)}Mbps` : `${settings.bitrate}kbps`} · {settings.encoder}</span>
		</div>
	);
}

function formatDur(s: number) {
	return `${String(Math.floor(s / 60)).padStart(2, "0")}:${String(s % 60).padStart(2, "0")}`;
}
