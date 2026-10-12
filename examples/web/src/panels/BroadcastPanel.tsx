import type { Kind } from "../config";
import { classify, findDataTrack } from "../moq/classify";
import { useBroadcast } from "../moq/useBroadcast";
import { useOptionalValue } from "../moq/useSignal";
import { DataPanel } from "./DataPanel";
import { Panel } from "./Panel";
import { VideoPanel } from "./VideoPanel";

/**
 * A panel for one broadcast. Without a `kind`, it reads the catalog first and becomes a video
 * or a data panel depending on what the broadcast carries.
 */
export function BroadcastPanel({ name, kind, onClose }: { name: string; kind?: Kind; onClose: () => void }) {
	if (kind === "video") return <VideoPanel name={name} onClose={onClose} />;
	return <CatalogPanel name={name} kind={kind} onClose={onClose} />;
}

function CatalogPanel({ name, kind, onClose }: { name: string; kind?: "data"; onClose: () => void }) {
	const broadcast = useBroadcast(name);
	const status = useOptionalValue(broadcast?.out.status);
	const catalog = useOptionalValue(broadcast?.out.catalog);

	if (broadcast && catalog) {
		const classified = classify(catalog);
		// Asked for data: take the data track even if the broadcast also has video.
		const data = kind === "data" ? findDataTrack(catalog) : classified.kind === "data" ? classified : undefined;
		if (data) return <DataPanel name={name} broadcast={broadcast} track={data} onClose={onClose} />;
		if (classified.kind === "video") return <VideoPanel name={name} onClose={onClose} />;
	}

	let line = "waiting for the broadcast…";
	if (!broadcast) line = "invalid relay URL";
	else if (catalog) line = kind === "data" ? "no data track in the catalog" : "nothing this viewer can show in the catalog";
	else if (status === "loading") line = "loading the catalog…";
	return <Panel name={name} kind="pending" status={line} onClose={onClose} />;
}
