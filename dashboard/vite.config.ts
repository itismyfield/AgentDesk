/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "path";
import { readFileSync } from "fs";

// Single source of truth for port/host defaults (shared with Rust backend & scripts)
const defaults = JSON.parse(readFileSync(path.resolve(__dirname, "../defaults.json"), "utf-8"));

function manualChunks(id: string) {
  if (!id.includes("node_modules")) return undefined;

  if (id.includes("react-dom") || id.includes("/react/")) {
    return "react";
  }

  if (id.includes("react-router-dom")) {
    return "router";
  }

  if (
    id.includes("react-markdown")
    || id.includes("remark-gfm")
    || id.includes("/remark-")
    || id.includes("/rehype-")
    || id.includes("/micromark")
    || id.includes("/mdast")
    || id.includes("/hast")
    || id.includes("/unist")
    || id.includes("/vfile")
  ) {
    return "markdown";
  }

  if (id.includes("lucide-react")) {
    return "icons";
  }

  if (id.includes("/gifuct-js/")) {
    return "pixi-gif";
  }

  if (id.includes("/earcut/")) {
    return "pixi-geom";
  }

  if (id.includes("/@xmldom/") || id.includes("/parse-svg-path/")) {
    return "pixi-svg";
  }

  if (id.includes("/eventemitter3/") || id.includes("/ismobilejs/") || id.includes("/tiny-lru/")) {
    return "pixi-utils";
  }

  if (id.includes("/pixi.js/") || id.includes("/@pixi/")) {
    return "pixi";
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
      "/api": `http://${defaults.loopback}:${defaults.port}`,
      "/ws": { target: `ws://${defaults.loopback}:${defaults.port}`, ws: true },
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
