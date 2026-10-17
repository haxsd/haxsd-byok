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
    if (path) throw new Error("configuration error: is not a Devin host file");
    return { path: "D:\\Windsurf\\resources\\app\\extensions\\windsurf\\dist\\extension.js" };
  };
  const probe = await probeHostPath("D:\\devin\\Devin", inspect);
  assert.deepEqual(calls, ["D:\\devin\\Devin", undefined]);
  assert.equal(probe.failure, null);
  assert.match(probe.status.path, /Windsurf/);
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
    return { path: "C:\\installed\\extension.js" };
  };
  const probe = await probeHostPath("   ", inspect);
  assert.deepEqual(calls, [undefined]);
  assert.equal(probe.failure, null);
});
