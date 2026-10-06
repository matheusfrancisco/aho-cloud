import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// `npm run dev` proxies Mage's server routes to a local `mage start server`; override with MAGE_SERVER.
const server = process.env.MAGE_SERVER ?? "http://127.0.0.1:8080";
const proxied = { target: server, changeOrigin: false };

export default defineConfig({
  plugins: [react()],
  // `@mage/web` and `@aho/ui` are linked packages with their own node_modules for their tests: one
  // copy of React and of the router for the shell and the pages.
  resolve: {
    dedupe: ["react", "react-dom", "react-router"],
  },
  server: {
    proxy: {
      "/api": proxied,
      "/auth": proxied,
      "/events": proxied,
      "/hooks": proxied,
      // GitHub/Linear/Slack install callbacks land on the server, not the SPA.
      "/integrations": proxied,
    },
  },
  // `mage-app` embeds this directory and serves it for `/`, `/login`, `/o/**`, short links and `/assets/**`.
  build: {
    outDir: "dist",
    assetsDir: "assets",
    emptyOutDir: true,
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./test/setup.ts"],
    include: ["test/**/*.test.{ts,tsx}"],
    restoreMocks: true,
  },
});
