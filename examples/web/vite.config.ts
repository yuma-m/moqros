import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
	plugins: [react()],
	// Relative asset paths, so the build works at / (nginx) and under /moqros/ (GitHub Pages).
	base: "./",
	server: { host: true, port: 8080 },
	preview: { host: true, port: 8080 },
	// @moq/watch bundles its decoders and polyfills into one chunk.
	build: { chunkSizeWarningLimit: 1024 },
});
