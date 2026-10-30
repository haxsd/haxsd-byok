import assert from "node:assert/strict";
import test from "node:test";
import { startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
test.after(() => loader.close());

test("port input never sends NaN or an out-of-range number", async () => {
  const { portInputValue } = await loader.load("/src/shared/utils/portInput.ts");

  assert.equal(portInputValue(""), 0);
  assert.equal(portInputValue("-"), 0);
  assert.equal(portInputValue("-1"), 0);
  assert.equal(portInputValue("65536"), 65_535);
  assert.equal(portInputValue("43110"), 43_110);
});

test("optional integer fields distinguish empty from invalid text", async () => {
  const { optionalPositiveInteger } = await loader.load("/src/shared/utils/portInput.ts");

  assert.equal(optionalPositiveInteger(""), null);
  assert.equal(optionalPositiveInteger("  "), null);
  assert.equal(optionalPositiveInteger("-"), null);
  assert.equal(optionalPositiveInteger("0"), 1);
  assert.equal(optionalPositiveInteger("64000"), 64_000);
});
