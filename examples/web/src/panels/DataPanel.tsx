import type { Broadcast } from "@moq/watch";
import { useMemo, useState } from "react";
import type { DataTrack } from "../moq/classify";
import { useDataTrack } from "../moq/useDataTrack";
import { Panel } from "./Panel";
import { renderersFor } from "./renderers";

const decoder = new TextDecoder();

/** Shows the newest message of one moqros data broadcast with a renderer picked by ROS type. */
export function DataPanel(props: { name: string; broadcast: Broadcast; track: DataTrack; onClose: () => void }) {
	const { name, broadcast, track, onClose } = props;
	const [paused, setPaused] = useState(false);
	const state = useDataTrack(broadcast, track, paused);

	const renderers = useMemo(() => renderersFor(track), [track]);
	const [choice, setChoice] = useState<string>();
	const renderer = renderers.find((entry) => entry.label === choice) ?? renderers[0];

	const parsed = useMemo(() => {
		if (track.format !== "json" || !state.payload) return { message: undefined };
		try {
			return { message: JSON.parse(decoder.decode(state.payload)) as unknown };
		} catch (err) {
			return { message: undefined, error: `invalid JSON: ${err instanceof Error ? err.message : err}` };
		}
	}, [track.format, state.payload]);

	const label = [track.rosType ?? "unknown type", track.format, track.mode].join(" · ");
	let line = `${label} · ${state.rate.toFixed(1)} msg/s · ${state.count} received`;
	if (paused) line += " · paused";
	if (state.ended) line += " · broadcast ended";
	if (state.error) line += ` · error: ${state.error}`;

	const Component = renderer?.component;
	return (
		<Panel
			name={name}
			kind="data"
			status={line}
			onClose={onClose}
			tools={
				<>
					{renderers.length > 1 && (
						<select value={renderer?.label} onChange={(event) => setChoice(event.target.value)} title="View">
							{renderers.map((entry) => (
								<option key={entry.label}>{entry.label}</option>
							))}
						</select>
					)}
					<button type="button" onClick={() => setPaused((value) => !value)}>
						{paused ? "Resume" : "Pause"}
					</button>
				</>
			}
		>
			<div className="data-body">
				{parsed.error ? (
					<div className="hint">{parsed.error}</div>
				) : state.payload && state.received !== undefined && Component ? (
					<Component
						name={name}
						track={track}
						payload={state.payload}
						message={parsed.message}
						received={state.received}
					/>
				) : (
					<div className="hint">waiting for a message…</div>
				)}
			</div>
		</Panel>
	);
}
