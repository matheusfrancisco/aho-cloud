import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  // `@aho/ui` is linked from `../../packages/ui`, which has its own node_modules for its tests: one
  // copy of React for the package and the pages.
  resolve: {
    dedupe: ["react", "react-dom", "react-router"],
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./test/setup.ts"],
    include: ["test/**/*.test.{ts,tsx}"],
    restoreMocks: true,
  },
});
