import { useMemo } from "react";
import { useConnection } from "./ConnectionContext";
import { useOptionalValue } from "./useSignal";

/** Names of the broadcasts the relay currently announces, sorted. */
export function useBroadcasts(): string[] {
	const connection = useConnection();
	const origin = useOptionalValue(connection?.origin);
	const broadcasts = useMemo(() => origin?.broadcasts(), [origin]);
	const routes = useOptionalValue(broadcasts);
	return useMemo(() => [...(routes?.keys() ?? [])].map(String).sort(), [routes]);
}
