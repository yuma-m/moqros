import type { Signals } from "@moq/watch";
import { useCallback, useSyncExternalStore } from "react";

/**
 * Read a signal that may not exist yet (e.g. before the connection or player is created).
 * `@moq/signals/react`'s `useValue` covers the case where the signal is always there.
 */
export function useOptionalValue<T>(signal: Signals.Getter<T> | undefined): T | undefined {
	const subscribe = useCallback((notify: () => void) => signal?.subscribe(() => notify()) ?? (() => {}), [signal]);
	return useSyncExternalStore(subscribe, () => signal?.peek());
}
