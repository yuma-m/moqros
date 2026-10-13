import type { Hang } from "@moq/watch";

export type DataFormat = "json" | "cdr";

/** A moqros data track, as `moqros::DataSettings::from_catalog` reads it. */
export interface DataTrack {
	track: string;
	format: DataFormat;
	mode: "snapshot" | "stream";
	/** ROS message type, e.g. `std_msgs/msg/Header`, when the publisher knows it. */
	rosType?: string;
}

export type Classified = { kind: "video" } | ({ kind: "data" } & DataTrack) | { kind: "unknown" };

/** Name of the single data track inside every moqros data broadcast. */
export const DATA_TRACK = "data";

/** MIME type of CDR payloads in the catalog's `binary` section. */
export const CDR_MIME = "application/x-ros2-cdr";

type Config = { mode: string; [key: string]: unknown };

function dataTrack(track: string, format: DataFormat, config: Config): DataTrack | undefined {
	if (config.mode !== "snapshot" && config.mode !== "stream") return undefined;
	const rosType = typeof config.rosType === "string" ? config.rosType : undefined;
	return { track, format, mode: config.mode, rosType };
}

/** The first moqros data track in the catalog: JSON before CDR. */
export function findDataTrack(catalog: Hang.Catalog.Root): DataTrack | undefined {
	for (const [track, config] of Object.entries(catalog.json?.tracks ?? {})) {
		const data = dataTrack(track, "json", config);
		if (data) return data;
	}
	for (const [track, config] of Object.entries(catalog.binary?.tracks ?? {})) {
		if (config.mime !== CDR_MIME) continue;
		const data = dataTrack(track, "cdr", config);
		if (data) return data;
	}
	return undefined;
}

/** What a broadcast carries, judged by its catalog. Video wins when it has both. */
export function classify(catalog: Hang.Catalog.Root): Classified {
	if (Object.keys(catalog.video?.renditions ?? {}).length > 0) return { kind: "video" };
	const data = findDataTrack(catalog);
	return data ? { kind: "data", ...data } : { kind: "unknown" };
}
