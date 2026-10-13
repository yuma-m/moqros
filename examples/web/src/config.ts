// The viewer state that lives in the query string, so a view can be bookmarked and shared.
//
//   url     relay URL (default: VITE_RELAY_URL at build time, else http://<page host>:4443/anon)
//   auto    0 to stop showing every announced broadcast (default: 1)
//   filter  only auto-show announced broadcasts whose name contains this text
//   show    comma-separated broadcasts always shown, as video or data depending on the catalog
//   video   comma-separated broadcasts always shown as video
//   data    comma-separated broadcasts always shown as data
//   hide    comma-separated announced broadcasts not to auto-show
//   teleop  broadcast to publish geometry_msgs/msg/Twist commands on, from a teleop pad
//
// `name` (one video broadcast) from the old static viewer is still read as `video`.
//
// Relay tokens stay out of the query string: the address bar gets the relay URL without them,
// and the full URL is kept in this tab's session storage so a reload still connects.

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
	/** Broadcast the teleop pad publishes on; no pad when unset. */
	teleop?: string;
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
		teleop: params.get("teleop")?.trim().replace(/^\/+/, "") || undefined,
	};
}

export function serializeConfig(config: Config): string {
	const params = new URLSearchParams();
	if (config.url !== defaultUrl()) params.set("url", stripToken(config.url));
	if (!config.auto) params.set("auto", "0");
	if (config.filter) params.set("filter", config.filter);
	const names = (kind?: Kind) => config.pinned.filter((pin) => pin.kind === kind).map((pin) => pin.name);
	if (names(undefined).length) params.set("show", names(undefined).join(","));
	if (names("video").length) params.set("video", names("video").join(","));
	if (names("data").length) params.set("data", names("data").join(","));
	if (config.hidden.length) params.set("hide", config.hidden.join(","));
	if (config.teleop) params.set("teleop", config.teleop);
	const query = params.toString();
	return query ? `?${query}` : "";
}

/** Keep the first entry for each broadcast name. */
export function dedupe(pinned: Pinned[]): Pinned[] {
	const seen = new Set<string>();
	return pinned.filter((pin) => !seen.has(pin.name) && seen.add(pin.name));
}

/** Whether a path segment looks like a JWT: three base64url parts, the first a JSON header (`eyJ`). */
function isJwt(segment: string): boolean {
	return segment.startsWith("eyJ") && segment.split(".").length === 3 && /^[A-Za-z0-9_.-]+$/.test(segment);
}

/**
 * The relay URL without its token: the `jwt` query parameter, or a JWT-shaped path segment
 * (Cloudflare's relays take the token in the path). Other URLs are returned unchanged.
 */
export function stripToken(url: string): string {
	let parsed: URL;
	try {
		parsed = new URL(url);
	} catch {
		return url;
	}
	const segments = parsed.pathname.split("/");
	const kept = segments.filter((segment) => !isJwt(segment));
	if (kept.length === segments.length && !parsed.searchParams.has("jwt")) return url;
	parsed.searchParams.delete("jwt");
	parsed.pathname = kept.join("/");
	return parsed.toString();
}

const TOKEN_KEY = "moqros-relay-url";

/** Keep a relay URL that carries a token in session storage, for {@link restoreToken}. */
export function rememberToken(url: string) {
	const stripped = stripToken(url);
	try {
		if (stripped === url) sessionStorage.removeItem(TOKEN_KEY);
		else sessionStorage.setItem(TOKEN_KEY, JSON.stringify({ stripped, url }));
	} catch {
		// Storage can be unavailable (e.g. blocked by the browser); a reload then needs the token again.
	}
}

/** The remembered URL with its token, if `url` is that URL without it; else `url`. */
export function restoreToken(url: string): string {
	try {
		const saved = JSON.parse(sessionStorage.getItem(TOKEN_KEY) ?? "null") as { stripped: string; url: string } | null;
		if (saved?.stripped === url) return saved.url;
	} catch {
		// Unavailable storage or a malformed entry: nothing to restore.
	}
	return url;
}
