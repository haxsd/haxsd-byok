import assert from "node:assert/strict";
import test from "node:test";
import { normalizeTauriUpdate } from "./normalize-tauri-update.mjs";

const REPOSITORY = "owner/repository";
const VERSION = "1.5.0";
const TAG = "haxsd-byok-v1.5.0";
const API_URL = `https://api.github.com/repos/${REPOSITORY}/releases/assets/7`;
const PUBLIC_URL = `https://github.com/${REPOSITORY}/releases/download/${TAG}/haxsd%20byok_1.5.0_x64-setup.exe`;

function manifest(url = API_URL) {
  return {
    version: VERSION,
    platforms: { "windows-x86_64": { signature: "signed-payload", url } },
  };
}

function release(tagName = TAG) {
  return {
    tag_name: tagName,
    assets: [
      {
        id: 7,
        name: "haxsd byok_1.5.0_x64-setup.exe",
        browser_download_url: `https://github.com/${REPOSITORY}/releases/download/${TAG}/installer.exe`,
      },
      // 没有 id/name 的条目是 GitHub 侧的噪声，跳过而不是让整个发布失败。
      { id: null, name: null },
    ],
  };
}

function normalize(target, options = {}) {
  return normalizeTauriUpdate(
    target,
    release(options.releaseTag ?? TAG),
    REPOSITORY,
    VERSION,
    options.tag ?? TAG,
  );
}

test("rewrites the metered API asset URL onto the public download URL", () => {
  const normalized = normalize(manifest());

  assert.equal(normalized.platforms["windows-x86_64"].url, PUBLIC_URL);
  assert.equal(normalized.platforms["windows-x86_64"].signature, "signed-payload");
});

test("keeps a URL that already points at a public asset", () => {
  const normalized = normalize(manifest(PUBLIC_URL));

  assert.equal(normalized.platforms["windows-x86_64"].url, PUBLIC_URL);
});

test("accepts this repository's tag prefix", () => {
  const normalized = normalize(manifest(), { tag: TAG, releaseTag: TAG });

  assert.match(
    normalized.platforms["windows-x86_64"].url,
    /\/releases\/download\/haxsd-byok-v1\.5\.0\//,
  );
});

test("defaults to the upstream v-prefixed tag", () => {
  const normalized = normalizeTauriUpdate(
    manifest(API_URL),
    release("v1.5.0"),
    REPOSITORY,
    VERSION,
  );

  assert.match(normalized.platforms["windows-x86_64"].url, /\/releases\/download\/v1\.5\.0\//);
});

test("rejects a manifest whose version does not match the release", () => {
  const mismatched = manifest();
  mismatched.version = "1.4.0";

  assert.throws(() => normalize(mismatched), /expected 1\.5\.0/);
});

test("rejects a release whose tag does not match the requested tag", () => {
  assert.throws(
    () => normalize(manifest(), { releaseTag: "haxsd-byok-v1.4.0" }),
    /expected haxsd-byok-v1\.5\.0/,
  );
});

test("rejects a platform that references an unknown asset", () => {
  const unknown = manifest(`https://github.com/${REPOSITORY}/releases/download/${TAG}/other.exe`);

  assert.throws(() => normalize(unknown), /unknown release asset/);
});

test("rejects a platform without a signature", () => {
  const unsigned = manifest();
  delete unsigned.platforms["windows-x86_64"].signature;

  assert.throws(() => normalize(unsigned), /missing its URL or signature/);
});

test("rejects a manifest without platforms", () => {
  assert.throws(() => normalize({ version: VERSION }), /no platforms/);
});
