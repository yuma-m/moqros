import { Broadcast, Net } from "@moq/watch";
import { useEffect, useState } from "react";
import { useConnection } from "./ConnectionContext";

/**
 * Subscribe to a broadcast's hang catalog on the page connection. The returned `Broadcast`
 * follows the broadcast across reconnects and republishes; read `out.status`, `out.catalog`
 * and `out.active` with `useOptionalValue`.
 */
export function useBroadcast(name: string): Broadcast | undefined {
	const connection = useConnection();
	const [broadcast, setBroadcast] = useState<Broadcast>();

	useEffect(() => {
		if (!connection) return;
		const broadcast = new Broadcast({
			origin: connection.origin,
			name: Net.Path.from(name),
			enabled: true,
			announced: true,
			// moqros publishes a hang catalog without the ".hang" name suffix.
			catalogFormat: "hang",
		});
		setBroadcast(broadcast);
		return () => {
			broadcast.close();
			setBroadcast(undefined);
		};
	}, [connection, name]);

	return broadcast;
}
