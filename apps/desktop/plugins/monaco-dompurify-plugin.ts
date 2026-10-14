import { fileURLToPath } from "node:url";
import type { Plugin } from "vite";

// Monaco 0.56 bundles an older DOMPurify source instead of importing its npm
// dependency. Resolve only that private import to the maintained ESM instance;
// both packages export createDOMPurify() with the same sanitizer/hook API.
export const monacoDOMPurifyPlugin: Plugin = {
  name: "monaco-managed-dompurify",
  enforce: "pre",
  // Dependency prebundling does not run this resolver inside Monaco. Keep the
  // package (including its deep imports) in Vite's normal module pipeline.
  config: () => ({ optimizeDeps: { exclude: ["monaco-editor"] } }),
  resolveId(source, importer) {
    const importingFile = importer?.split("?", 1)[0].replaceAll("\\", "/");
    if (source === "./dompurify/dompurify.js"
      && importingFile?.endsWith("/monaco-editor/esm/vs/base/browser/domSanitize.js")) {
      return fileURLToPath(import.meta.resolve("dompurify"));
    }
  },
};
