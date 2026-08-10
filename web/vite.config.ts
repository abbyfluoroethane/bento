import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    // The Go control plane serves /api; proxy it during development.
    proxy: { "/api": "http://localhost:8080" },
  },
  build: {
    outDir: "dist",
    // The bundle is embedded into the Go binary; keep it inspectable.
    sourcemap: false,
  },
});
