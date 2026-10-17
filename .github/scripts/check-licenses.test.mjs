import assert from "node:assert/strict";
import test from "node:test";
import {
  ALLOWED_LICENSES,
  checkCargoPackages,
  checkLockfile,
  licenseVerdict,
  tokenize,
} from "./check-licenses.mjs";

function allowed(expression) {
  return licenseVerdict(expression).allowed;
}

test("accepts the expression shapes this workspace actually contains", () => {
  // 这些是 `cargo metadata` 在本仓库里真实出现过的写法。
  for (const expression of [
    "MIT",
    "MIT OR Apache-2.0",
    "MIT/Apache-2.0",
    "Apache-2.0 OR MIT",
    "Apache-2.0/MIT",
    "MIT / Apache-2.0",
    "Unlicense OR MIT",
    "Unlicense/MIT",
    "Zlib OR Apache-2.0 OR MIT",
    "BSD-3-Clause OR Apache-2.0",
    "BSD-3-Clause/MIT",
    "0BSD OR MIT OR Apache-2.0",
    "CC0-1.0 OR MIT-0 OR Apache-2.0",
    "ISC AND (Apache-2.0 OR ISC)",
    "MIT OR Apache-2.0 OR Zlib",
    "Apache-2.0 OR ISC OR MIT",
    "BSD-2-Clause OR Apache-2.0 OR MIT",
    "(MIT OR Apache-2.0) AND Apache-2.0",
    "(Apache-2.0 OR MIT) AND BSD-3-Clause",
    "(MIT OR Apache-2.0) AND Unicode-3.0",
    "ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0)",
    "MIT AND BSD-3-Clause",
    "Apache-2.0 AND MIT",
    "Apache-2.0 AND ISC",
    "BSD-3-Clause AND MIT",
    "Apache-2.0 OR BSL-1.0",
    "Apache-2.0 OR BSL-1.0 OR MIT",
    "MPL-2.0",
    "CDLA-Permissive-2.0",
    "Unicode-3.0",
  ]) {
    assert.ok(allowed(expression), `${expression} should be allowed`);
  }
});

test("rejects copyleft and unknown licenses", () => {
  for (const expression of ["GPL-3.0-only", "AGPL-3.0", "SSPL-1.0", "BUSL-1.1"]) {
    assert.ok(!allowed(expression), `${expression} should be rejected`);
  }
  assert.deepEqual(licenseVerdict("GPL-3.0-only").unmet, ["GPL-3.0-only"]);
  // 双许可里只要有一条允许分支就可用：`MIT OR GPL-2.0` 可以按 MIT 使用。
  assert.ok(allowed("MIT OR GPL-2.0"), "a dual license with an allowed branch is usable");
  assert.ok(!allowed("MIT AND GPL-2.0"), "AND needs both branches");
});

test("an AND needs every branch allowed, an OR needs one", () => {
  assert.ok(allowed("MIT AND Apache-2.0"));
  assert.ok(!allowed("MIT AND GPL-3.0-only"));
  assert.ok(allowed("GPL-3.0-only OR MIT"));
  assert.equal(allowed("MIT AND Apache-2.0"), true);
});

test("WITH clauses only relax the base license", () => {
  assert.ok(allowed("Apache-2.0 WITH LLVM-exception"));
  assert.ok(allowed("Apache-2.0 WITH LLVM-exception OR MIT"));
  assert.ok(!allowed("GPL-3.0-only WITH Classpath-exception-2.0"));
  assert.deepEqual(tokenize("Apache-2.0 WITH LLVM-exception"), ["Apache-2.0"]);
});

test("a broken expression is a violation, not a pass", () => {
  for (const expression of ["", "MIT AND", "(MIT", "MIT)", "MIT GPL-3.0-only"]) {
    const verdict = licenseVerdict(expression);
    assert.ok(!verdict.allowed, `${JSON.stringify(expression)} must not pass`);
    assert.ok(verdict.reason, `${JSON.stringify(expression)} needs a reason`);
  }
});

test("the allow list stays explicit and uppercase", () => {
  for (const license of ALLOWED_LICENSES) {
    assert.equal(license, license.toUpperCase(), `${license} must be canonical`);
  }
  assert.ok(ALLOWED_LICENSES.has("MIT") && ALLOWED_LICENSES.has("APACHE-2.0"));
  assert.ok(!ALLOWED_LICENSES.has("GPL-3.0-ONLY"));
});

test("cargo packages outside the workspace are checked and path packages skipped", () => {
  const metadata = {
    workspace_members: ["our-crate 0.1.0 (path+file:///repo/server)"],
    packages: [
      { id: "our-crate 0.1.0 (path+file:///repo/server)", name: "our-crate", version: "0.1.0", source: null },
      { id: "good 1.0.0 (registry+x)", name: "good", version: "1.0.0", source: "registry+x", license: "MIT OR Apache-2.0" },
      { id: "bad 2.0.0 (registry+x)", name: "bad", version: "2.0.0", source: "registry+x", license: "GPL-3.0-only" },
      { id: "mystery 3.0.0 (registry+x)", name: "mystery", version: "3.0.0", source: "registry+x" },
    ],
  };

  const result = checkCargoPackages(metadata);

  assert.equal(result.checked, 3);
  assert.deepEqual(result.violations, ["bad 2.0.0: GPL-3.0-only (unmet: GPL-3.0-only)"]);
  assert.deepEqual(result.unverifiable, ["mystery 3.0.0"]);
});

test("the frontend lock is checked per package and reports unrecorded licenses", () => {
  const lock = {
    packages: {
      "": { name: "app", version: "1.0.0", license: "MIT" },
      "node_modules/good": { version: "1.0.0", license: "Apache-2.0 OR MIT" },
      "node_modules/bad": { version: "1.0.0", license: "AGPL-3.0" },
      "node_modules/unknown": { version: "1.0.0" },
      "node_modules/linked": { version: "1.0.0", link: true },
    },
  };

  const result = checkLockfile(lock);

  // 根条目（""）与 link 条目不算依赖。
  assert.equal(result.checked, 3);
  assert.deepEqual(result.violations, ["node_modules/bad: AGPL-3.0 (unmet: AGPL-3.0)"]);
  assert.deepEqual(result.unverifiable, ["node_modules/unknown"]);
});
