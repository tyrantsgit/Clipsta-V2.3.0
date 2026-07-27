import { useEffect, useState } from "react";
import { CheckCircle, X } from "lucide-react";

export default function SaveNotification({ path }: { path: string | null }) {
	const [visible, setVisible] = useState(false);
	const [current, setCurrent] = useState<string | null>(null);

	useEffect(() => {
		if (!path) return;
		setCurrent(path);
		setVisible(true);
		const t = setTimeout(() => setVisible(false), 4000);
		return () => clearTimeout(t);
	}, [path]);

	if (!visible || !current) return null;

	const name = current.split(/[\\/]/).pop() ?? current;

	return (
		<div className="fixed bottom-10 right-6 z-50 slide-in">
			<div className="bg-card border border-[#2e2e00] rounded-xl px-4 py-3 flex items-center gap-3 shadow-2xl max-w-xs">
				<CheckCircle size={20} className="text-y flex-shrink-0" />
				<div className="flex-1 min-w-0">
					<p className="text-white text-sm font-semibold">Clip saved!</p>
					<p className="text-text-mid text-xs truncate">{name}</p>
				</div>
				<button onClick={() => setVisible(false)} className="text-text-dim hover:text-white">
					<X size={14} />
				</button>
			</div>
		</div>
	);
}
