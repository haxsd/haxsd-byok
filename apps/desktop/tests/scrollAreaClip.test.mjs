import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";

test("the scroll area shell cannot become a second programmatic scroll container", () => {
  const source = fs.readFileSync(
    new URL("../src/shared/scrollable/ScrollArea.module.scss", import.meta.url),
    "utf8",
  );
  const rootRule = source.slice(0, source.indexOf(".viewport"));

  assert.match(rootRule, /overflow:\s*hidden/);
  assert.match(rootRule, /overflow:\s*clip/);
  assert.match(source, /\.viewport\s*\{[^}]*overflow-y:\s*auto/s);
});
