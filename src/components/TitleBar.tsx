import { Minus, Square, X } from "lucide-react";
import bridge from "../tauri-bridge";

export default function TitleBar() {
	return (
		<div className="drag h-9 bg-[#080808] border-b border-border flex items-center justify-between px-4 flex-shrink-0">
			<span className="text-text-dim text-xs select-none">Clipsta Desktop</span>
			<div className="flex items-center no-drag">
				<WinBtn onClick={() => bridge.minimize()} hover="hover:bg-muted">
					<Minus size={12} />
				</WinBtn>
				<WinBtn onClick={() => bridge.maximize()} hover="hover:bg-muted">
					<Square size={11} />
				</WinBtn>
				<WinBtn onClick={() => bridge.close()} hover="hover:bg-red-600">
					<X size={12} />
				</WinBtn>
			</div>
		</div>
	);
}

function WinBtn({ children, onClick, hover }: { children: React.ReactNode; onClick: () => void; hover: string }) {
	return (
		<button
			onClick={onClick}
			className={`w-10 h-9 flex items-center justify-center text-text-mid ${hover} transition-colors`}
		>
			{children}
		</button>
	);
}
