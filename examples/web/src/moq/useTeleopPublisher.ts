import { Hang, Net, Signals } from "@moq/watch";
import { useCallback, useEffect, useRef, useState } from "react";
import { DATA_TRACK } from "./classify";
import { useConnection } from "./ConnectionContext";

/** geometry_msgs/msg/Twist in r2r's JSON mapping. */
export interface Twist {
	linear: { x: number; y: number; z: number };
	angular: { x: number; y: number; z: number };
}

export const TWIST_TYPE = "geometry_msgs/msg/Twist";

/** How long the broadcast outlives the stop command it sends last. */
const STOP_GRACE_MS = 300;

export function twist(linear: number, angular: number): Twist {
	// Every field: r2r deserializes the whole message, so a missing one fails on the robot.
	return { linear: { x: linear, y: 0, z: 0 }, angular: { x: 0, y: 0, z: angular } };
}

/**
 * Publish a moqros JSON data broadcast of `Twist` messages, the way `moqros-pub --format json`
 * does: a hang catalog naming the ROS type, and a `data` track with one message per group
 * (snapshot mode), so a subscriber that falls behind jumps to the newest command.
 * `moqros-sub` (or `moqros-bridge --broadcast`) republishes it on ROS.
 *
 * The broadcast goes into the page's shared origin, so the connection announces it to the relay
 * and again after each reconnect. It exists only while `active`: when several pages publish the
 * same name, the relay keeps routing subscribers to the one that announced first, so an idle page
 * must step aside for the one in use. Returns `send`, which is a no-op while not `ready`.
 */
export function useTeleopPublisher(name: string, active: boolean): { send: (message: Twist) => void; ready: boolean } {
	const connection = useConnection();
	const track = useRef<Net.Track.Producer>(undefined);
	const [ready, setReady] = useState(false);

	useEffect(() => {
		if (!connection || !active) return;
		const effect = new Signals.Effect((effect) => {
			const origin = effect.get(connection.origin);
			if (!origin) return;

			const broadcast = origin.createBroadcast(Net.Path.from(name));
			const catalog = broadcast.createTrack(Hang.Catalog.TRACK, { priority: Hang.Catalog.PRIORITY.catalog });
			const group = catalog.appendGroup();
			// What moqros::DataSettings::from_catalog reads; see crates/moqros/src/data.rs.
			group.writeJson({
				json: { tracks: { [DATA_TRACK]: { mode: "snapshot", schema: `ros2:${TWIST_TYPE}`, rosType: TWIST_TYPE } } },
			});
			group.close();
			track.current = broadcast.createTrack(DATA_TRACK, { priority: Hang.Catalog.PRIORITY.text });
			broadcast.announce();
			setReady(true);

			effect.cleanup(() => {
				// Leave the robot stopped rather than wait for its own timeout. The stop and the
				// retraction travel on different streams, so close a little later: a subscriber
				// that sees the retraction first would drop the stop. Recreating the broadcast
				// meanwhile is fine; the origin replaces this one, and closing it late doesn't
				// retract its successor.
				const stop = track.current?.appendGroup();
				stop?.writeJson(twist(0, 0));
				stop?.close();
				track.current = undefined;
				setReady(false);
				setTimeout(() => broadcast.close(), STOP_GRACE_MS);
			});
		});
		return () => effect.close();
	}, [connection, name, active]);

	const send = useCallback((message: Twist) => {
		const group = track.current?.appendGroup();
		if (!group) return;
		group.writeJson(message);
		group.close();
	}, []);

	return { send, ready };
}
