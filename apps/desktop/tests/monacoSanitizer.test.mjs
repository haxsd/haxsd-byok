import assert from "node:assert/strict";
import { test } from "node:test";
import path from "node:path";
import { createRequire } from "node:module";
import { readFile } from "node:fs/promises";
import { build, resolveConfig } from "vite";
import { desktopRoot } from "./harness.mjs";

const require = createRequire(import.meta.url);
const sanitizerEntry = require.resolve("monaco-editor/base/browser/domSanitize.js");

for (const config of ["vite.config.ts", "vite.demo.config.ts"]) {
  test(`${config} excludes Monaco prebundling and resolves versioned dev imports`, async () => {
    const resolved = await resolveConfig({ root: desktopRoot, configFile: path.join(desktopRoot, config), logLevel: "silent" }, "serve");
    const plugin = resolved.plugins.find((item) => item.name === "monaco-managed-dompurify");
    assert.ok(plugin);
    const target = await plugin.resolveId.call({}, "./dompurify/dompurify.js", `${sanitizerEntry}?v=regression`);
    assert.deepEqual({
      excludesMonacoPrebundling: resolved.optimizeDeps.exclude.includes("monaco-editor"),
      queryImportUsesManagedSanitizer: typeof target === "string" && target.replaceAll("\\", "/").includes("/node_modules/dompurify/dist/purify.es.mjs"),
    }, {
      excludesMonacoPrebundling: true,
      queryImportUsesManagedSanitizer: true,
    }, "development must not bypass the maintained sanitizer through prebundling or versioned importer paths");
    const code = await readFile(target, "utf8");
    assert.ok(/\.version\s*=\s*["']3\.4\.16["']/.test(code));
    assert.ok(!/\.version\s*=\s*["']3\.4\.8["']/.test(code));
  });

  test(`${config} bundles the maintained DOMPurify in Monaco's real sanitization path`, async () => {
    const result = await build({
      root: desktopRoot,
      configFile: path.join(desktopRoot, config),
      logLevel: "silent",
      build: {
        write: false,
        minify: false,
        rollupOptions: {
          input: sanitizerEntry,
          preserveEntrySignatures: "strict",
        },
      },
    });
    const outputs = (Array.isArray(result) ? result : [result]).flatMap((bundle) => bundle.output);
    const code = outputs.filter((item) => item.type === "chunk").map((item) => item.code).join("\n");
    assert.ok(/\.version\s*=\s*["']3\.4\.16["']/.test(code), "Monaco must use the patched sanitizer, not only list it in package-lock.json");
    assert.ok(!/\.version\s*=\s*["']3\.4\.8["']/.test(code), "the vendored, unpatched sanitizer must not survive bundling");
    assert.ok(/function sanitizeHtml\(/.test(code), "the test must build Monaco's actual sanitization entry");
  });
}
