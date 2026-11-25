/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "path";
import { readFileSync } from "fs";

// Single source of truth for port/host defaults (shared with Rust backend & scripts)
const defaults = JSON.parse(readFileSync(path.resolve(__dirname, "../defaults.json"), "utf-8"));

function manualChunks(rawId: string) {
  const id = rawId.replace(/\0/g, "");
  const marker = "/node_modules/";
  const idx = id.lastIndexOf(marker);
  if (idx === -1) return undefined;
  const pkg = id.slice(idx + marker.length);

  if (
    pkg.startsWith("react/")
    || pkg.startsWith("react-dom/")
    || pkg.startsWith("scheduler/")
  ) {
    return "react";
  }

  if (pkg.startsWith("react-router/")) {
    return "router";
  }

  if (pkg.startsWith("lucide-react/")) {
    return "icons";
  }

  return undefined;
}

export default defineConfig({
  test: {
    exclude: ["e2e/**", "node_modules/**"],
  },
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "src") },
  },
  server: {
    port: 5173,
    proxy: {
      "/api": { target: `http://${defaults.loopback}:${defaults.port}`, changeOrigin: false },
      "/ws": { target: `ws://${defaults.loopback}:${defaults.port}`, ws: true, changeOrigin: false },
    },
  },
  build: {
    outDir: "dist",
    chunkSizeWarningLimit: 600,
    rollupOptions: {
      output: {
        manualChunks,
      },
    },
  },
});
