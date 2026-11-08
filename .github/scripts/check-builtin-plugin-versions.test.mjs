import test from "node:test";
import assert from "node:assert/strict";

import {
  pluginsRequiringVersionBump,
  versionFromManifest,
} from "./check-builtin-plugin-versions.mjs";

test("version comes from a valid plugin manifest", () => {
  assert.equal(versionFromManifest('{"version":"1.2.3"}'), "1.2.3");
  assert.equal(versionFromManifest("{}"), undefined);
  assert.equal(versionFromManifest("not json"), undefined);
  assert.equal(versionFromManifest(undefined), undefined);
});

test("content changes without a bump are reported", () => {
  assert.deepEqual(
    pluginsRequiringVersionBump([
      { plugin: "codex-auth", changed: true, oldVersion: "1.0.0", newVersion: "1.0.0" },
      { plugin: "grok-auth", changed: true, oldVersion: "1.0.0", newVersion: "1.0.1" },
    ]),
    ["codex-auth"],
  );
});

test("new plugins and deleted manifests are not treated as missing bumps", () => {
  assert.deepEqual(
    pluginsRequiringVersionBump([
      { plugin: "fresh", changed: true, oldVersion: undefined, newVersion: "0.1.0" },
      { plugin: "removed", changed: true, oldVersion: "0.1.0", newVersion: undefined },
      { plugin: "untouched", changed: false, oldVersion: "0.1.0", newVersion: "0.1.0" },
    ]),
    [],
  );
});
