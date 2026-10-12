import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
	plugins: [react()],
	server: { host: true, port: 8080 },
	preview: { host: true, port: 8080 },
	// @moq/watch bundles its decoders and polyfills into one chunk.
	build: { chunkSizeWarningLimit: 1024 },
});
