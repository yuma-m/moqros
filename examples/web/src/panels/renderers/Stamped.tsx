import type { RendererProps } from "./registry";

interface Stamp {
	sec: number;
	nanosec: number;
}

function isStamp(value: unknown): value is Stamp {
	const stamp = value as Partial<Stamp> | undefined;
	return typeof stamp?.sec === "number" && typeof stamp?.nanosec === "number";
}

/** Find `header.stamp` (or a bare `stamp`) in r2r's JSON mapping of a message. */
function stampOf(message: unknown): Stamp | undefined {
	const object = message as { stamp?: unknown; header?: { stamp?: unknown } } | undefined;
	const stamp = object?.header?.stamp ?? object?.stamp;
	return isStamp(stamp) ? stamp : undefined;
}

/**
 * Example of a custom renderer: for any message with a header, show its stamp and how old it
 * was on arrival. The age compares the robot's clock with this machine's, so it's only
 * meaningful when the two are synchronized. (Docker Desktop's VM clock can be off from the
 * host's by a second or so.)
 */
export function Stamped(props: RendererProps) {
	const stamp = stampOf(props.message);
	if (!stamp) return <div className="hint">no header.stamp in this message</div>;
	const ms = stamp.sec * 1000 + stamp.nanosec / 1e6;
	// Wall-clock arrival time. `performance.timeOrigin` can drift from the wall clock (e.g. across
	// a laptop's sleep), so go back from `Date.now()` instead.
	const arrival = Date.now() - (performance.now() - props.received);
	const frameId = (props.message as { header?: { frame_id?: string }; frame_id?: string }).header?.frame_id ??
		(props.message as { frame_id?: string }).frame_id;
	return (
		<dl className="stamped">
			<dt>stamp</dt>
			<dd>{new Date(ms).toISOString()}</dd>
			<dt>age on arrival</dt>
			<dd>{(arrival - ms).toFixed(1)} ms</dd>
			{frameId !== undefined && (
				<>
					<dt>frame_id</dt>
					<dd>{frameId || "(empty)"}</dd>
				</>
			)}
		</dl>
	);
}
