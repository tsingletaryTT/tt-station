// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Served by tt-station-agentd at /app (src/app_ui.rs), same origin as the API.
// `npm run dev` proxies the API to the local agent.
export default defineConfig({
  base: "/app/",
  plugins: [react()],
  build: { outDir: "dist", chunkSizeWarningLimit: 1500 },
  server: {
    port: 5173,
    proxy: Object.fromEntries(
      ["/status", "/serve", "/modes", "/run", "/stop", "/tt", "/telemetry", "/assistant", "/metrics", "/doctor",
       "/events", "/clients", "/connect", "/keys", "/orchard", "/jobs", "/people", "/schedules", "/energy",
       "/remote", "/power", "/software", "/lab", "/generate", "/creations", "/docs", "/memories", "/logs",
       "/leases", "/coserve", "/bench", "/pair"].map((p) => [p, { target: "http://127.0.0.1:8765", ws: true }]),
    ),
  },
});
