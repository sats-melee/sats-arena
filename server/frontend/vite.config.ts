import { defineConfig } from "vite";

export default defineConfig({
  base: "./",
  server: {
    proxy: {
      "/rooms": { target: "ws://127.0.0.1:8738", ws: true },
      "/health": "http://127.0.0.1:8738",
    },
  },
});
