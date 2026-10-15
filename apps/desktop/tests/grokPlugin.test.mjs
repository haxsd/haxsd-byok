// Run the pure Grok plugin suite through the project's existing Vite/Node harness.
// Only Deno.test registration and SDK aliases are supplied; production modules run unchanged.
import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

const root = fileURLToPath(new URL("../", import.meta.url));
const repoRoot = fileURLToPath(new URL("../../../", import.meta.url));
const configUrl = new URL("../../../server/plugins/build-in/grok-auth/deno.json", import.meta.url);
const { imports } = JSON.parse(await readFile(configUrl, "utf8"));
const alias = Object.fromEntries(Object.entries(imports).map(([name, relative]) => [
  name, fileURLToPath(new URL(relative, configUrl)),
]));

test("Grok account plugin behavior", { timeout: 20_000 }, async (suite) => {
  const server = await createServer({
    configFile: false,
    root,
    resolve: { alias },
    appType: "custom",
    logLevel: "error",
    server: { middlewareMode: true, hmr: false, ws: false, watch: null, fs: { allow: [repoRoot] } },
  });
  const cases = [];
  const previousDeno = globalThis.Deno;
  globalThis.Deno = { test: (name, run) => cases.push({ name, run }) };
  try {
    const entry = `/@fs/${fileURLToPath(new URL("grok_test.ts", configUrl)).replace(/\\/g, "/")}`;
    await server.ssrLoadModule(entry);
    assert.ok(cases.length > 0, "the plugin suite did not register any behavior tests");
    for (const { name, run } of cases) await suite.test(name, run);
  } finally {
    if (previousDeno === undefined) delete globalThis.Deno;
    else globalThis.Deno = previousDeno;
    await server.close();
  }
});
