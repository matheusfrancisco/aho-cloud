import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// `npm run dev` proxies API calls to a local control plane; override with AHO_CLOUD_API.
const api = process.env.AHO_CLOUD_API ?? "http://127.0.0.1:8080";

export default defineConfig({
  plugins: [react()],
  // `@aho/ui` is linked from `../packages/ui`, which has its own node_modules for its tests: one copy
  // of React (and of xterm, so tests can mock it) for the console and the package.
  resolve: {
    dedupe: ["react", "react-dom", "@xterm/xterm", "@xterm/addon-fit"],
  },
  server: {
    proxy: {
      "/v1": { target: api, changeOrigin: true },
    },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./test/setup.ts"],
    include: ["test/**/*.test.{ts,tsx}"],
    restoreMocks: true,
  },
});
