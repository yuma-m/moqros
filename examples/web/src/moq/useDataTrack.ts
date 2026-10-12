import { type Broadcast, Signals } from "@moq/watch";
import { useEffect, useRef, useState } from "react";
import type { DataTrack } from "./classify";

export interface DataState {
	/** The newest message payload. */
	payload?: Uint8Array;
	/** When `payload` arrived (`performance.now()`). */
	received?: number;
	/** Messages received since the panel opened. */
	count: number;
	/** Messages per second over the last second or so. */
	rate: number;
	ended: boolean;
	error?: string;
}

const INITIAL: DataState = { count: 0, rate: 0, ended: false };

/**
 * Read every frame of a moqros data track (one frame = one ROS message) and keep the newest.
 * Resubscribes whenever the broadcast comes back. Updates are batched to one per animation
 * frame, so a fast topic doesn't re-render the panel for every message.
 */
export function useDataTrack(broadcast: Broadcast | undefined, data: DataTrack | undefined, paused: boolean): DataState {
	const [state, setState] = useState<DataState>(INITIAL);
	// Pausing keeps the last message on screen; resuming carries on counting from it.
	const kept = useRef(state);
	kept.current = state;
	const track = data?.track;

	useEffect(() => {
		if (!broadcast || !track || paused) return;
		let latest: DataState = kept.current;
		let scheduled = 0;
		const flush = () => {
			scheduled = 0;
			setState(latest);
		};
		const update = (next: Partial<DataState>) => {
			latest = { ...latest, ...next };
			scheduled ||= requestAnimationFrame(flush);
		};

		let windowCount = 0;
		let windowStart = performance.now();

		const effect = new Signals.Effect((effect) => {
			const active = effect.get(broadcast.out.active);
			if (!active) return;
			const subscription = active.track(track).subscribe({ priority: 90 });
			effect.cleanup(() => subscription.close());
			const reader = subscription.ordered();
			update({ ended: false, error: undefined });

			effect.spawn(async () => {
				try {
					for (let frame; (frame = await reader.readFrame()); ) {
						windowCount++;
						const now = performance.now();
						const elapsed = (now - windowStart) / 1000;
						const rate = elapsed >= 1 ? windowCount / elapsed : latest.rate;
						if (elapsed >= 1) {
							windowCount = 0;
							windowStart = now;
						}
						update({ payload: frame.payload, received: now, count: latest.count + 1, rate });
					}
					update({ ended: true, rate: 0 });
				} catch (err) {
					if (!effect.abort.aborted) update({ error: err instanceof Error ? err.message : String(err), rate: 0 });
				}
			});
		});

		// Show 0 msg/s while messages stop coming.
		const decay = setInterval(() => {
			if (latest.rate !== 0 && performance.now() - (latest.received ?? 0) > 2000) update({ rate: 0 });
		}, 1000);

		return () => {
			effect.close();
			clearInterval(decay);
			cancelAnimationFrame(scheduled);
		};
	}, [broadcast, track, paused]);

	return state;
}
