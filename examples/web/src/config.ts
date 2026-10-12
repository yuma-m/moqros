// The viewer state that lives in the query string, so a view can be bookmarked and shared.
//
//   url     relay URL (default: VITE_RELAY_URL at build time, else http://<page host>:4443/anon)
//   auto    0 to stop showing every announced broadcast (default: 1)
//   filter  only auto-show announced broadcasts whose name contains this text
//   show    comma-separated broadcasts always shown, as video or data depending on the catalog
//   video   comma-separated broadcasts always shown as video
//   data    comma-separated broadcasts always shown as data
//   hide    comma-separated announced broadcasts not to auto-show
//
// `name` (one video broadcast) from the old static viewer is still read as `video`.

export type Kind = "video" | "data";

export interface Pinned {
	name: string;
	/** Forced by the user; otherwise read from the catalog. */
	kind?: Kind;
}

export interface Config {
	url: string;
	auto: boolean;
	filter: string;
	pinned: Pinned[];
	hidden: string[];
}

function list(value: string | null): string[] {
	return (value ?? "")
		.split(",")
		.map((item) => item.trim())
		.filter(Boolean);
}

export function defaultUrl(): string {
	// The GitHub Pages build sets this to the local relay; the page host there is github.io.
	const configured = import.meta.env.VITE_RELAY_URL;
	if (configured) return configured;
	return `http://${location.hostname || "localhost"}:4443/anon`;
}

export function parseConfig(search: string): Config {
	const params = new URLSearchParams(search);
	const video = [...list(params.get("video")), ...list(params.get("name"))];
	const pinned: Pinned[] = list(params.get("show")).map((name) => ({ name }));
	for (const name of video) pinned.push({ name, kind: "video" });
	for (const name of list(params.get("data"))) pinned.push({ name, kind: "data" });
	return {
		url: params.get("url") || defaultUrl(),
		auto: params.get("auto") !== "0",
		filter: params.get("filter") ?? "",
		pinned: dedupe(pinned),
		hidden: list(params.get("hide")),
	};
}

export function serializeConfig(config: Config): string {
	const params = new URLSearchParams();
	if (config.url !== defaultUrl()) params.set("url", config.url);
	if (!config.auto) params.set("auto", "0");
	if (config.filter) params.set("filter", config.filter);
	const names = (kind?: Kind) => config.pinned.filter((pin) => pin.kind === kind).map((pin) => pin.name);
	if (names(undefined).length) params.set("show", names(undefined).join(","));
	if (names("video").length) params.set("video", names("video").join(","));
	if (names("data").length) params.set("data", names("data").join(","));
	if (config.hidden.length) params.set("hide", config.hidden.join(","));
	const query = params.toString();
	return query ? `?${query}` : "";
}

/** Keep the first entry for each broadcast name. */
export function dedupe(pinned: Pinned[]): Pinned[] {
	const seen = new Set<string>();
	return pinned.filter((pin) => !seen.has(pin.name) && seen.add(pin.name));
}
