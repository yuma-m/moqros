import type { RendererProps } from "./registry";

const PREVIEW = 256;

/** Fallback for CDR tracks: the browser can't decode ROS CDR, so show the size and a hex dump. */
export function CdrBytes({ payload }: RendererProps) {
	const rows: string[] = [];
	for (let offset = 0; offset < Math.min(payload.byteLength, PREVIEW); offset += 16) {
		const bytes = [...payload.subarray(offset, offset + 16)].map((byte) => byte.toString(16).padStart(2, "0"));
		rows.push(`${offset.toString(16).padStart(4, "0")}  ${bytes.join(" ")}`);
	}
	if (payload.byteLength > PREVIEW) rows.push("…");
	return (
		<>
			<div className="hint">{payload.byteLength} bytes of CDR</div>
			<pre>{rows.join("\n")}</pre>
		</>
	);
}
