import type { RendererProps } from "./registry";

function Value({ value, label, depth }: { value: unknown; label?: string; depth: number }) {
	const key = label !== undefined ? <span className="json-key">{label}: </span> : null;
	if (value === null || typeof value !== "object") {
		const kind = value === null ? "null" : typeof value;
		return (
			<div className="json-leaf">
				{key}
				<span className={`json-${kind}`}>{JSON.stringify(value)}</span>
			</div>
		);
	}

	const entries = Array.isArray(value) ? value.map((item, i) => [String(i), item] as const) : Object.entries(value);
	const summary = Array.isArray(value) ? `[${value.length}]` : `{${entries.length}}`;
	// Long arrays (ranges, covariances) start collapsed.
	const open = depth < 3 && !(Array.isArray(value) && value.length > 16);
	return (
		<details className="json-node" open={open}>
			<summary>
				{key}
				<span className="json-summary">{summary}</span>
			</summary>
			{entries.map(([k, v]) => (
				<Value key={k} label={k} value={v} depth={depth + 1} />
			))}
		</details>
	);
}

/** Default renderer for JSON tracks: the newest message as a collapsible tree. */
export function JsonTree({ message }: RendererProps) {
	return (
		<div className="json-tree">
			<Value value={message} depth={0} />
		</div>
	);
}

/** The newest message as indented JSON text. */
export function JsonText({ message }: RendererProps) {
	return <pre>{JSON.stringify(message, null, 2)}</pre>;
}
