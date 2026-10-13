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

/** nav_msgs/msg/Odometry of a ground robot: planar position, heading, and speeds. */
export function Odometry(props: RendererProps) {
	const odom = props.message as Odom | undefined;
	const position = odom?.pose?.pose?.position;
	const orientation = odom?.pose?.pose?.orientation;
	if (!position || !orientation) return <div className="hint">no pose in this message</div>;
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
				{(twist?.linear?.x ?? 0).toFixed(2)} m/s · {(twist?.angular?.z ?? 0).toFixed(2)} rad/s
			</dd>
			<dt>frames</dt>
			<dd>
				{odom.header?.frame_id || "(empty)"} → {odom.child_frame_id || "(empty)"}
			</dd>
		</dl>
	);
}
