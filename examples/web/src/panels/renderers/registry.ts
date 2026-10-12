import type { ComponentType } from "react";
import type { DataTrack } from "../../moq/classify";

/** What a renderer gets: the newest message of one data broadcast. */
export interface RendererProps {
	/** The broadcast name. */
	name: string;
	track: DataTrack;
	/** Raw message payload: UTF-8 JSON for `json` tracks, ROS CDR for `cdr` tracks. */
	payload: Uint8Array;
	/** The payload parsed as JSON, for `json` tracks. */
	message?: unknown;
	/** When the payload arrived (`performance.now()`). */
	received: number;
}

export type Renderer = ComponentType<RendererProps>;

interface Entry {
	/**
	 * A ROS type (`std_msgs/msg/Header`), a package wildcard (`sensor_msgs/msg/*`), `*` for any
	 * track (even one without a type), or a regular expression over the type.
	 */
	rosType: string | RegExp;
	format?: DataTrack["format"];
	label: string;
	component: Renderer;
}

const entries: Entry[] = [];

/**
 * Register a renderer for a ROS message type. Later registrations win, so an application can
 * override the built-in ones. Data panels let the viewer switch between every renderer that
 * matches, so a custom view never hides the raw message.
 *
 * ```ts
 * register({ rosType: "sensor_msgs/msg/Imu", format: "json", label: "IMU", component: ImuView });
 * ```
 */
export function register(entry: Entry): void {
	entries.unshift(entry);
}

function matches(pattern: string | RegExp, rosType: string | undefined): boolean {
	if (pattern === "*") return true;
	if (!rosType) return false;
	if (pattern instanceof RegExp) return pattern.test(rosType);
	if (pattern.endsWith("/*")) return rosType.startsWith(pattern.slice(0, -1));
	return pattern === rosType;
}

/** Every renderer for this track, most specific first. */
export function renderersFor(track: DataTrack): Entry[] {
	const candidates = entries.filter(
		(entry) => (!entry.format || entry.format === track.format) && matches(entry.rosType, track.rosType),
	);
	const rank = ({ rosType }: Entry) =>
		rosType === "*" ? 2 : typeof rosType === "string" && rosType.endsWith("/*") ? 1 : 0;
	return candidates.sort((a, b) => rank(a) - rank(b));
}
