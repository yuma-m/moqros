import type { RendererProps } from "./registry";

interface Odom {
	header?: { frame_id?: string };
	child_frame_id?: string;
	pose?: { pose?: { position?: { x: number; y: number }; orientation?: { x: number; y: number; z: number; w: number } } };
	twist?: { twist?: { linear?: { x: number }; angular?: { z: number } } };
}

/** Yaw (rotation about z) of a quaternion, in radians. */
function yaw({ x, y, z, w }: { x: number; y: number; z: number; w: number }): number {
	return Math.atan2(2 * (w * z + x * y), 1 - 2 * (y * y + z * z));
}

// The message is whatever the broadcast's publisher sent, so check the fields that are formatted
// below at runtime: calling `toFixed` on a non-number, or rendering an object, would throw.

/** Whether `value` has the numeric `x` and `y` the position line formats. */
function isPosition(value: unknown): value is { x: number; y: number } {
	const position = value as { x?: unknown; y?: unknown } | undefined;
	return typeof position?.x === "number" && typeof position?.y === "number";
}

/** `value` if it's a number, else 0 (as for a missing field). */
function numberOr0(value: unknown): number {
	return typeof value === "number" ? value : 0;
}

/** A frame id for display; an object or array (React can't render objects) is shown as JSON. */
function frameText(value: unknown): string | number | boolean {
	if (typeof value === "object" && value !== null) return JSON.stringify(value);
	return (value as string | number | boolean | undefined) || "(empty)";
}

/** nav_msgs/msg/Odometry of a ground robot: planar position, heading, and speeds. */
export function Odometry(props: RendererProps) {
	const odom = props.message as Odom | undefined;
	const position = odom?.pose?.pose?.position;
	const orientation = odom?.pose?.pose?.orientation;
	if (!position || !orientation || !isPosition(position)) return <div className="hint">no pose in this message</div>;
	const twist = odom.twist?.twist;
	return (
		<dl className="stamped">
			<dt>position</dt>
			<dd>
				x {position.x.toFixed(2)} m · y {position.y.toFixed(2)} m
			</dd>
			<dt>heading</dt>
			<dd>{((yaw(orientation) * 180) / Math.PI).toFixed(1)}°</dd>
			<dt>speed</dt>
			<dd>
				{numberOr0(twist?.linear?.x).toFixed(2)} m/s · {numberOr0(twist?.angular?.z).toFixed(2)} rad/s
			</dd>
			<dt>frames</dt>
			<dd>
				{frameText(odom.header?.frame_id)} → {frameText(odom.child_frame_id)}
			</dd>
		</dl>
	);
}
