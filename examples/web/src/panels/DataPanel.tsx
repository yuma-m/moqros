import type { Broadcast } from "@moq/watch";
import { Component as ReactComponent, type ReactNode, useMemo, useState } from "react";
import type { DataTrack } from "../moq/classify";
import { useDataTrack } from "../moq/useDataTrack";
import { Panel } from "./Panel";
import { renderersFor } from "./renderers";
import type { Renderer } from "./renderers/registry";

const decoder = new TextDecoder();

interface BoundaryProps {
	/** The message being rendered; a new one clears a failure. */
	payload: Uint8Array;
	/** The renderer; switching to another one clears a failure. */
	view: Renderer;
	children: ReactNode;
}

interface BoundaryState {
	failed: boolean;
	message?: string;
	payload: Uint8Array;
	view: Renderer;
}

/**
 * Keeps a renderer that throws on a message (which comes from whoever publishes the broadcast)
 * from unmounting the whole app: only this panel's body shows the failure. The next message,
 * or another view, tries again.
 */
class RendererBoundary extends ReactComponent<BoundaryProps, BoundaryState> {
	state: BoundaryState = { failed: false, payload: this.props.payload, view: this.props.view };

	static getDerivedStateFromError(error: unknown): Partial<BoundaryState> {
		return { failed: true, message: error instanceof Error && typeof error.message === "string" ? error.message : undefined };
	}

	static getDerivedStateFromProps(props: BoundaryProps, state: BoundaryState): Partial<BoundaryState> | null {
		if (props.payload === state.payload && props.view === state.view) return null;
		return { failed: false, message: undefined, payload: props.payload, view: props.view };
	}

	render() {
		if (!this.state.failed) return this.props.children;
		const detail = this.state.message ? `: ${this.state.message}` : "";
		return <div className="hint">this view failed to render the message{detail}</div>;
	}
}

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
					<RendererBoundary payload={state.payload} view={Component}>
						<Component
							name={name}
							track={track}
							payload={state.payload}
							message={parsed.message}
							received={state.received}
						/>
					</RendererBoundary>
				) : (
					<div className="hint">waiting for a message…</div>
				)}
			</div>
		</Panel>
	);
}
