import { useEffect, useMemo, useState } from "react";
import { BroadcastList } from "./components/BroadcastList";
import { SettingsBar } from "./components/SettingsBar";
import { type Config, dedupe, type Kind, type Pinned, parseConfig, serializeConfig } from "./config";
import { ConnectionProvider } from "./moq/ConnectionContext";
import { useBroadcasts } from "./moq/useBroadcasts";
import { BroadcastPanel } from "./panels/BroadcastPanel";

export function App() {
	const [config, setConfig] = useState(() => parseConfig(location.search));

	// Mirror the view into the address bar so it can be bookmarked or shared.
	useEffect(() => {
		const query = serializeConfig(config);
		if (query !== location.search) history.replaceState(null, "", `${location.pathname}${query}${location.hash}`);
	}, [config]);

	return (
		<ConnectionProvider url={config.url}>
			<Viewer config={config} setConfig={setConfig} />
		</ConnectionProvider>
	);
}

function Viewer({ config, setConfig }: { config: Config; setConfig: (update: (config: Config) => Config) => void }) {
	const announced = useBroadcasts();

	// Panels: the pinned broadcasts, then (in auto mode) every other announced one that passes
	// the filter and wasn't hidden.
	const shown = useMemo(() => {
		const panels: Pinned[] = [...config.pinned];
		if (config.auto) {
			for (const name of announced) {
				if (config.hidden.includes(name) || !name.includes(config.filter)) continue;
				panels.push({ name });
			}
		}
		const seen = new Set<string>();
		return panels.filter((panel) => !seen.has(panel.name) && seen.add(panel.name));
	}, [config, announced]);

	const show = (name: string, kind?: Kind) =>
		setConfig((config) => ({
			...config,
			hidden: config.hidden.filter((hidden) => hidden !== name),
			// Announced broadcasts in auto mode need no pin unless the user forced a kind.
			pinned:
				kind || !config.auto || !announced.includes(name) || !name.includes(config.filter)
					? dedupe([...config.pinned.filter((pin) => pin.name !== name), { name, kind }])
					: config.pinned,
		}));

	const hide = (name: string) =>
		setConfig((config) => ({
			...config,
			pinned: config.pinned.filter((pin) => pin.name !== name),
			hidden: config.auto && announced.includes(name) ? [...new Set([...config.hidden, name])] : config.hidden,
		}));

	return (
		<div className="app">
			<header className="top">
				<h1>moqros viewer</h1>
				<SettingsBar
					url={config.url}
					onUrl={(url) => setConfig((config) => ({ ...config, url }))}
					onAdd={(name, kind) => show(name, kind)}
				/>
			</header>
			<div className="layout">
				<BroadcastList
					announced={announced}
					shown={new Set(shown.map((panel) => panel.name))}
					auto={config.auto}
					filter={config.filter}
					onAuto={(auto) => setConfig((config) => ({ ...config, auto }))}
					onFilter={(filter) => setConfig((config) => ({ ...config, filter }))}
					onToggle={(name, on) => (on ? show(name) : hide(name))}
				/>
				<main className="panels">
					{shown.length === 0 && <div className="hint">Nothing to show yet. Add a broadcast or wait for one to be announced.</div>}
					{shown.map((panel) => (
						<BroadcastPanel
							key={`${config.url} ${panel.name} ${panel.kind ?? ""}`}
							name={panel.name}
							kind={panel.kind}
							onClose={() => hide(panel.name)}
						/>
					))}
				</main>
			</div>
			<footer>
				Video panels play <code>moqros-pub</code> image topics; data panels show the newest message of topics sent
				with <code>format=json</code> (or the size of <code>cdr</code> ones). An <code>http://</code> relay URL makes
				the browser fetch the relay's self-signed certificate fingerprint from <code>/certificate.sha256</code>; use
				it for local development only.
			</footer>
		</div>
	);
}
