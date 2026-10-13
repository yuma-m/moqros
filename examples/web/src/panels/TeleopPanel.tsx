import { useCallback, useEffect, useRef, useState } from "react";
import { twist, useTeleopPublisher } from "../moq/useTeleopPublisher";
import { Panel } from "./Panel";

type Direction = "forward" | "back" | "left" | "right";

const KEYS: Record<string, Direction> = {
	ArrowUp: "forward",
	KeyW: "forward",
	ArrowDown: "back",
	KeyS: "back",
	ArrowLeft: "left",
	KeyA: "left",
	ArrowRight: "right",
	KeyD: "right",
};

/** How often a held command is repeated. The robot stops when commands stop coming. */
const RATE_MS = 100;

/** Whether this page is the one in use: visible and focused. */
function useInUse(): boolean {
	const check = () => document.visibilityState === "visible" && document.hasFocus();
	const [inUse, setInUse] = useState(check);
	useEffect(() => {
		const update = () => setInUse(check());
		window.addEventListener("focus", update);
		window.addEventListener("blur", update);
		document.addEventListener("visibilitychange", update);
		return () => {
			window.removeEventListener("focus", update);
			window.removeEventListener("blur", update);
			document.removeEventListener("visibilitychange", update);
		};
	}, []);
	return inUse;
}

/** Whether a key press belongs to a form field rather than to the pad. */
function typing(target: EventTarget | null): boolean {
	return target instanceof HTMLElement && (target.isContentEditable || /^(INPUT|SELECT|TEXTAREA)$/.test(target.tagName));
}

/**
 * Drive a robot: publishes `geometry_msgs/msg/Twist` on the broadcast `name` while a direction
 * is held, from the pad or the keyboard (WASD or arrow keys, Space to stop). Up and down set
 * `linear.x`, left and right `angular.z`. Releasing every direction sends one zero command.
 *
 * Only the page in use (visible and focused) publishes, so the newest tab or window takes over
 * the robot, and one left in the background can't hold on to it.
 */
export function TeleopPanel({ name, onClose }: { name: string; onClose: () => void }) {
	const inUse = useInUse();
	const { send, ready } = useTeleopPublisher(name, inUse);
	const [held, setHeld] = useState<ReadonlySet<Direction>>(new Set());
	const [maxLinear, setMaxLinear] = useState(0.5);
	const [maxAngular, setMaxAngular] = useState(1.0);
	const [sent, setSent] = useState(0);

	const linear = ((held.has("forward") ? 1 : 0) - (held.has("back") ? 1 : 0)) * maxLinear;
	const angular = ((held.has("left") ? 1 : 0) - (held.has("right") ? 1 : 0)) * maxAngular;
	const moving = linear !== 0 || angular !== 0;

	const publish = useRef(() => {});
	publish.current = () => {
		send(twist(linear, angular));
		setSent((sent) => sent + 1);
	};

	// Send right away on every change (including the stop), then repeat while moving.
	useEffect(() => {
		if (!ready) return;
		publish.current();
		if (!moving) return;
		const timer = setInterval(() => publish.current(), RATE_MS);
		return () => clearInterval(timer);
	}, [ready, moving, linear, angular]);

	const press = useCallback((direction: Direction, down: boolean) => {
		setHeld((held) => {
			if (held.has(direction) === down) return held;
			const next = new Set(held);
			if (down) next.add(direction);
			else next.delete(direction);
			return next;
		});
	}, []);
	const stop = useCallback(() => setHeld(new Set()), []);

	useEffect(() => {
		const key = (down: boolean) => (event: KeyboardEvent) => {
			if (typing(event.target) || event.metaKey || event.ctrlKey || event.altKey) return;
			if (event.code === "Space") {
				if (down) stop();
			} else if (KEYS[event.code]) {
				press(KEYS[event.code], down);
			} else {
				return;
			}
			event.preventDefault();
		};
		const keydown = key(true);
		const keyup = key(false);
		// A key released while the page is in the background never sends keyup.
		const blur = () => stop();
		window.addEventListener("keydown", keydown);
		window.addEventListener("keyup", keyup);
		window.addEventListener("blur", blur);
		return () => {
			window.removeEventListener("keydown", keydown);
			window.removeEventListener("keyup", keyup);
			window.removeEventListener("blur", blur);
		};
	}, [press, stop]);

	const button = (direction: Direction, label: string) => (
		<button
			type="button"
			className={held.has(direction) ? "active" : undefined}
			onPointerDown={(event) => {
				event.currentTarget.setPointerCapture(event.pointerId);
				press(direction, true);
			}}
			onPointerUp={() => press(direction, false)}
			onPointerCancel={() => press(direction, false)}
			onLostPointerCapture={() => press(direction, false)}
		>
			{label}
		</button>
	);

	let status = `linear ${linear.toFixed(2)} m/s · angular ${angular.toFixed(2)} rad/s · ${sent} sent`;
	if (!inUse) status = "paused: click this page to take control";
	else if (!ready) status = "waiting for the relay…";

	return (
		<Panel name={name} kind="teleop" status={status} onClose={onClose}>
			<div className="teleop">
				<div className="pad">
					<span />
					{button("forward", "↑")}
					<span />
					{button("left", "←")}
					<button type="button" className="stop" onClick={stop} title="Stop (Space)">
						■
					</button>
					{button("right", "→")}
					<span />
					{button("back", "↓")}
					<span />
				</div>
				<div className="speeds">
					<label>
						Linear {maxLinear.toFixed(2)} m/s
						<input
							type="range"
							min={0.1}
							max={1}
							step={0.05}
							value={maxLinear}
							onChange={(event) => setMaxLinear(Number(event.target.value))}
						/>
					</label>
					<label>
						Angular {maxAngular.toFixed(2)} rad/s
						<input
							type="range"
							min={0.1}
							max={1.5}
							step={0.05}
							value={maxAngular}
							onChange={(event) => setMaxAngular(Number(event.target.value))}
						/>
					</label>
					<p className="hint">
						Hold WASD or the arrow keys, or the pad. Space stops. Sends <code>geometry_msgs/msg/Twist</code> every{" "}
						{RATE_MS} ms while held.
					</p>
				</div>
			</div>
		</Panel>
	);
}
