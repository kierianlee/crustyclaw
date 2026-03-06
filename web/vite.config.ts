import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { tanstackRouter } from "@tanstack/router-plugin/vite";

export default defineConfig({
  plugins: [tailwindcss(), tanstackRouter(), react()],
  server: {
    proxy: {
      "/api": "http://localhost:11111",
    },
  },
  resolve: {
    alias: {
      "@": "/src",
    },
  },
});
