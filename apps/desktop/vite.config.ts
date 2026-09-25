import { codeInspectorPlugin } from "code-inspector-plugin";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { staticI18nPlugin } from "./plugins/static-i18n-plugin.ts";

const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(async ({ command }) => ({
  base: "/__byok-api__/",
  plugins: [
    staticI18nPlugin(),
    ...(command === "serve"
      ? [codeInspectorPlugin({
          bundler: "vite",
          editor: "cursor",
          hotKeys: ["ctrlKey"],
          pathType: "absolute",
          ...(process.platform === "darwin" ? { launchType: "open" as const } : {}),
        })]
      : []),
    react(),
  ],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  build: {
    rollupOptions: {
      output: {
        // 图表库占了入口包的四分之三，和应用代码捆在一起时它每次都要跟着重新解析。
        // 这里只把它们分开，不改变加载顺序：两个 chunk 并行下载。
        manualChunks(id: string) {
          if (!id.includes("node_modules")) return undefined;
          if (id.includes("echarts") || id.includes("zrender")) return "charts";
          if (/[\\/]node_modules[\\/](react|react-dom|react-router|react-router-dom|scheduler)[\\/]/.test(id)) return "react";
          return undefined;
        },
      },
    },
  },
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || "127.0.0.1",
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
