/// <reference types="vite/client" />

interface ImportMetaEnv {
	/** Default relay URL; unset means http://<page host>:4443/anon. */
	readonly VITE_RELAY_URL?: string;
}
