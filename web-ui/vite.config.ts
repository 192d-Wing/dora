import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/health": "http://10.10.10.251:3333",
      "/ready": "http://10.10.10.251:3333",
      "/v1": "http://10.10.10.251:3333",
      "/metrics": "http://10.10.10.251:3333",
    },
  },
});
