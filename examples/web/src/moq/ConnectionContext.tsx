import { Net } from "@moq/watch";
import { createContext, type ReactNode, useContext, useEffect, useState } from "react";

const ConnectionContext = createContext<Net.Connection | undefined>(undefined);

/**
 * One relay connection for the whole page. It reconnects on its own; every panel reads its
 * broadcasts through `connection.origin`, so panels follow it across reconnects.
 */
export function ConnectionProvider({ url, children }: { url: string; children: ReactNode }) {
	const [connection, setConnection] = useState<Net.Connection>();

	useEffect(() => {
		let parsed: URL;
		try {
			parsed = new URL(url);
		} catch {
			setConnection(undefined);
			return;
		}
		const connection = new Net.Connection({ url: parsed, enabled: true });
		setConnection(connection);
		return () => connection.close();
	}, [url]);

	return <ConnectionContext.Provider value={connection}>{children}</ConnectionContext.Provider>;
}

export function useConnection(): Net.Connection | undefined {
	return useContext(ConnectionContext);
}
