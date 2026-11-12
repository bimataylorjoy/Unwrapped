import { defineConfig } from "vite";

export default defineConfig({
  // Relative asset paths so the build works from GitHub Pages or any subfolder.
  base: "./",
  build: { target: "es2022" },
  // The app reads ../deployments/testnet.json and the SDK sources.
  server: { fs: { allow: [".."] } },
});
