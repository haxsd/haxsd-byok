// Enforces the built-in plugin rule: changing a plugin's content requires a version bump.
//
// A built-in plugin's version is the only way clients and the takeover layer can tell two
// builds apart; the 2026-10 plugin outage was a content change shipped without one. CI runs
// this against the push/PR base commit and fails when files under `server/plugins/build-in/
// <plugin>/` changed while its `plugin.json` version stayed the same.
//
// Usage: node .github/scripts/check-builtin-plugin-versions.mjs --base <git-sha>
// Without a usable base (e.g. the first push of a branch, all-zero sha) the check is skipped.

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

export const PLUGIN_ROOT = "server/plugins/build-in";

export function versionFromManifest(text) {
  if (text === null || text === undefined) return undefined;
  try {
    const parsed = JSON.parse(text);
    return typeof parsed.version === "string" ? parsed.version : undefined;
  } catch {
    return undefined;
  }
}

/** Plugin ids that changed files but kept the same manifest version. */
export function pluginsRequiringVersionBump(entries) {
  const required = new Set();
  for (const entry of entries) {
    if (!entry.changed) continue;
    if (entry.oldVersion === undefined) continue; // a new plugin has nothing to compare
    if (entry.newVersion === undefined) continue; // manifest deleted: the plugin is gone
    if (entry.oldVersion === entry.newVersion) required.add(entry.plugin);
  }
  return [...required].sort();
}

function runGit(args) {
  return execFileSync("git", args, { encoding: "utf8" });
}

function changedPluginFiles(base) {
  const output = runGit([
    "diff",
    "--name-status",
    base,
    "HEAD",
    "--",
    PLUGIN_ROOT,
  ]);
  const files = [];
  for (const line of output.split("\n")) {
    if (!line.trim()) continue;
    const [status, ...paths] = line.split("\t");
    const path = paths[paths.length - 1];
    if (!path) continue;
    files.push({ status, path });
  }
  return files;
}

function pluginIdOf(path) {
  const prefix = `${PLUGIN_ROOT}/`;
  if (!path.startsWith(prefix)) return undefined;
  const rest = path.slice(prefix.length);
  const id = rest.split("/")[0];
  return id || undefined;
}

function manifestAt(reference, plugin) {
  try {
    return runGit(["show", `${reference}:${PLUGIN_ROOT}/${plugin}/plugin.json`]);
  } catch {
    return undefined;
  }
}

const isFullSha = (value) => /^[0-9a-f]{40}$/i.test(value ?? "");
const isZeroSha = (value) => /^0{40}$/.test(value ?? "");

function main(argv) {
  const baseIndex = argv.indexOf("--base");
  const base =
    baseIndex >= 0
      ? argv[baseIndex + 1]
      : process.env.BASE_SHA ?? process.argv[2];
  if (!isFullSha(base) || isZeroSha(base)) {
    console.log(
      `plugin version check skipped: no usable base commit (${base ?? "missing"})`,
    );
    process.exit(0);
  }

  const files = changedPluginFiles(base);
  const changedPlugins = new Map();
  for (const file of files) {
    const plugin = pluginIdOf(file.path);
    if (plugin) changedPlugins.set(plugin, true);
  }

  const entries = [...changedPlugins.keys()].map((plugin) => ({
    plugin,
    changed: true,
    oldVersion: versionFromManifest(manifestAt(base, plugin)),
    newVersion: versionFromManifest(
      (() => {
        try {
          return readFileSync(`${PLUGIN_ROOT}/${plugin}/plugin.json`, "utf8");
        } catch {
          return undefined;
        }
      })(),
    ),
  }));
  const required = pluginsRequiringVersionBump(entries);
  if (required.length > 0) {
    console.error(
      "built-in plugin content changed without a plugin.json version bump:",
    );
    for (const plugin of required) {
      console.error(`  - ${PLUGIN_ROOT}/${plugin}`);
    }
    console.error(
      "bump the version in plugin.json (and any catalog entry that references it)",
    );
    process.exit(1);
  }
  console.log(
    `plugin version check passed (${changedPlugins.size} changed built-in plugin(s))`,
  );
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  main(process.argv.slice(2));
}
