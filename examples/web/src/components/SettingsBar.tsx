import { type FormEvent, useEffect, useState } from "react";
import type { Kind } from "../config";
import { useConnection } from "../moq/ConnectionContext";
import { useOptionalValue } from "../moq/useSignal";

/** Relay URL, connection status, and a form to add a broadcast by name. */
export function SettingsBar(props: { url: string; onUrl: (url: string) => void; onAdd: (name: string, kind?: Kind) => void }) {
	const connection = useConnection();
	const status = useOptionalValue(connection?.status);
	const error = useOptionalValue(connection?.error);

	const [url, setUrl] = useState(props.url);
	useEffect(() => setUrl(props.url), [props.url]);

	const [name, setName] = useState("");
	const [kind, setKind] = useState<Kind | "auto">("auto");

	const connect = (event: FormEvent) => {
		event.preventDefault();
		props.onUrl(url.trim());
	};
	const add = (event: FormEvent) => {
		event.preventDefault();
		const trimmed = name.trim().replace(/^\/+/, "");
		if (!trimmed) return;
		props.onAdd(trimmed, kind === "auto" ? undefined : kind);
		setName("");
	};

	return (
		<div className="settings">
			<form onSubmit={connect}>
				<label>
					Relay URL
					<input value={url} onChange={(event) => setUrl(event.target.value)} spellCheck={false} />
				</label>
				<button type="submit">Connect</button>
				<span className={`connection connection-${connection ? status : "invalid"}`} title={error?.message}>
					{connection ? status : "invalid URL"}
					{error && status !== "connected" ? `: ${error.message}` : ""}
				</span>
			</form>
			<form onSubmit={add}>
				<label>
					Broadcast
					<input
						value={name}
						onChange={(event) => setName(event.target.value)}
						placeholder="camera/image_raw"
						spellCheck={false}
					/>
				</label>
				<label>
					As
					<select value={kind} onChange={(event) => setKind(event.target.value as Kind | "auto")}>
						<option value="auto">auto</option>
						<option value="video">video</option>
						<option value="data">data</option>
					</select>
				</label>
				<button type="submit">Add</button>
			</form>
		</div>
	);
}
