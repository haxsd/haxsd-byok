import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
installBrowserDoubles();
test.after(() => loader.close());

const { probeHostPath } = await loader.load("/src/features/devin/hostPathProbe.ts");

test("a remembered host path that no longer resolves falls back to detection", async () => {
  const calls = [];
  const inspect = async (path) => {
    calls.push(path);
    if (path) throw new Error("configuration error: is not a Devin settings file or data directory");
    return { path: "D:\\Users\\me\\AppData\\Roaming\\Devin\\User\\settings.json" };
  };
  const probe = await probeHostPath("D:\\devin\\Devin", inspect);
  assert.deepEqual(calls, ["D:\\devin\\Devin", undefined]);
  assert.equal(probe.failure, null);
  assert.deepEqual(probe.matched, { path: false, installRoot: false });
  assert.match(probe.status.path, /Devin[\\/]User[\\/]settings\.json/);
});

test("a working remembered path is used as-is", async () => {
  const calls = [];
  const inspect = async (path) => {
    calls.push(path);
    return { path };
  };
  const probe = await probeHostPath("D:\\devin\\Devin", inspect);
  assert.deepEqual(calls, ["D:\\devin\\Devin"]);
  assert.equal(probe.status.path, "D:\\devin\\Devin");
});

test("when neither the remembered path nor detection resolves the reason is returned, not thrown", async () => {
  const inspect = async () => {
    throw new Error("no Devin installation was found");
  };
  const probe = await probeHostPath("D:\\devin\\Devin", inspect);
  assert.equal(probe.status, null);
  assert.match(probe.failure, /no Devin installation/);
});

test("with nothing remembered the page asks detection directly", async () => {
  const calls = [];
  const inspect = async (path) => {
    calls.push(path);
    return { path: "C:\\Users\\me\\AppData\\Roaming\\Devin\\User\\settings.json" };
  };
  const probe = await probeHostPath("   ", inspect);
  assert.deepEqual(calls, [undefined]);
  assert.equal(probe.failure, null);
});

test("a stale installation root does not discard a working settings path", async () => {
  const path = "D:\\devin\\User\\settings.json";
  const root = "D:\\old\\Devin";
  const calls = [];
  const inspect = async (settingsPath, installRoot) => {
    calls.push([settingsPath, installRoot]);
    if (installRoot) throw new Error("Devin installation path not found");
    return { path: settingsPath };
  };

  const probe = await probeHostPath(path, inspect, root);

  assert.deepEqual(calls, [
    [path, root],
    [undefined, root],
    [path, undefined],
  ], "先保住安装目录，再单独回退设置路径");
  assert.equal(probe.status.path, path);
  assert.deepEqual(probe.matched, { path: true, installRoot: false });
});

test("a stale settings path does not discard a working installation root", async () => {
  const root = "D:\\Devin";
  const detected = "C:\\Users\\me\\AppData\\Roaming\\Devin\\User\\settings.json";
  const calls = [];
  const inspect = async (settingsPath, installRoot) => {
    calls.push([settingsPath, installRoot]);
    if (settingsPath) throw new Error("is not a Devin settings file or data directory");
    return { path: detected };
  };

  const probe = await probeHostPath("D:\\uninstalled\\settings.json", inspect, root);

  assert.deepEqual(calls, [
    ["D:\\uninstalled\\settings.json", root],
    [undefined, root],
  ]);
  assert.equal(probe.status.path, detected);
  assert.deepEqual(probe.matched, { path: false, installRoot: true });
});

test("an installation root is forwarded when no settings path is remembered", async () => {
  const root = "D:\\Devin";
  const calls = [];
  const inspect = async (settingsPath, installRoot) => {
    calls.push([settingsPath, installRoot]);
    return { path: "C:\\Users\\me\\AppData\\Roaming\\Devin\\User\\settings.json" };
  };

  const probe = await probeHostPath("  ", inspect, ` ${root} `);

  assert.deepEqual(calls, [[undefined, root]]);
  assert.deepEqual(probe.matched, { path: false, installRoot: true });
});
