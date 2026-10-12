import { Net, Player, Signals } from "@moq/watch";
import { useEffect, useRef, useState } from "react";
import { useConnection } from "../moq/ConnectionContext";
import { useOptionalValue } from "../moq/useSignal";
import { Panel } from "./Panel";

/** Frames per second from the decoder's running frame count. */
function useFps(player: Player | undefined): number {
	const [fps, setFps] = useState(0);
	useEffect(() => {
		if (!player) return;
		let last = player.video.out.stats.peek()?.frameCount ?? 0;
		const timer = setInterval(() => {
			const count = player.video.out.stats.peek()?.frameCount ?? 0;
			setFps(Math.max(0, count - last));
			last = count;
		}, 1000);
		return () => clearInterval(timer);
	}, [player]);
	return fps;
}

/** Plays one hang video broadcast into a canvas, on the page's shared connection. */
export function VideoPanel({ name, onClose }: { name: string; onClose: () => void }) {
	const connection = useConnection();
	const canvas = useRef<HTMLCanvasElement>(null);
	const [player, setPlayer] = useState<Player>();
	const [paused] = useState(() => new Signals.Signal(false));

	useEffect(() => {
		if (!connection || !canvas.current) return;
		const player = new Player({
			origin: connection.origin,
			probe: connection.probe,
			name: Net.Path.from(name),
			enabled: true,
			announced: true,
			// moqros publishes a hang catalog without the ".hang" name suffix.
			catalogFormat: "hang",
			canvas: canvas.current,
			paused,
			// Robot video: paint each frame as soon as it decodes. There's no audio to sync with.
			delay: "instant",
			muted: true,
			// Stop downloading while the panel is scrolled away or the tab is hidden.
			visible: "100px",
		});
		setPlayer(player);
		return () => {
			player.close();
			setPlayer(undefined);
		};
	}, [connection, name, paused]);

	const status = useOptionalValue(player?.broadcast.out.status);
	const display = useOptionalValue(player?.video.out.display);
	const stalled = useOptionalValue(player?.video.out.stalled);
	const isPaused = useOptionalValue(paused);
	const fps = useFps(player);

	let line: string;
	if (!connection) line = "invalid relay URL";
	else if (status !== "live") line = status === "loading" ? "loading the catalog…" : "waiting for the broadcast…";
	else if (!display) line = "waiting for a keyframe…";
	else line = `${display.width}×${display.height} · ${fps} fps${stalled ? " · stalled" : ""}`;

	return (
		<Panel
			name={name}
			kind="video"
			status={line}
			onClose={onClose}
			tools={
				<button type="button" onClick={() => paused.update((value) => !value)}>
					{isPaused ? "Play" : "Pause"}
				</button>
			}
		>
			<canvas ref={canvas} style={display && { aspectRatio: `${display.width} / ${display.height}` }} />
		</Panel>
	);
}
