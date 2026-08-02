import { execFileSync } from "node:child_process";

import { reactRouter } from "@react-router/dev/vite";
import { defineConfig } from "vite";

// Fall back to git only when the version isn't supplied in an env var.
process.env.VITE_TML_CONSOLE_REV ??= (() => {
  try {
    return execFileSync("git", ["describe", "--tags", "--always", "--dirty"], {
      encoding: "utf8",
    }).trim();
  } catch {
    return "unknown";
  }
})();

export default defineConfig({
  plugins: [reactRouter()],
  server: {
    // Dev-only: forward API calls to a local switchboard (e.g. `nix run
    // .#devstack`), so the SPA runs same-origin with an empty VITE_TML_API_URL.
    proxy: {
      "/api": {
        target: process.env.TML_DEV_PROXY ?? "http://127.0.0.1:8081",
        changeOrigin: true,
      },
    },
  },
});
