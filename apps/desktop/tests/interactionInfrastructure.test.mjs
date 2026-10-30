// Keyboard and modal boundaries shared by every page.

import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

test("only the top modal layer may handle Escape or trap Tab", async () => {
  const { isTopModalLayer, pushModalLayer, removeModalLayer } = await loader.load("/src/shared/ui/modalLayer.ts");
  const parent = Symbol("parent");
  const child = Symbol("child");

  pushModalLayer(parent);
  pushModalLayer(child);
  assert.equal(isTopModalLayer(parent), false);
  assert.equal(isTopModalLayer(child), true);

  removeModalLayer(child);
  assert.equal(isTopModalLayer(parent), true);
  removeModalLayer(parent);
  assert.equal(isTopModalLayer(parent), false);
});

test("the calendar moves by week or weekday without creating 365 tab stops", async () => {
  const { calendarTargetIndex } = await loader.load("/src/features/home/charts/calendarNavigation.ts");
  const cells = Array.from({ length: 14 }, (_, index) => ({ row: index % 7 }));

  assert.equal(calendarTargetIndex(cells, 3, "ArrowRight"), 10);
  assert.equal(calendarTargetIndex(cells, 3, "ArrowLeft"), 0);
  assert.equal(calendarTargetIndex(cells, 3, "ArrowDown"), 4);
  assert.equal(calendarTargetIndex(cells, 3, "ArrowUp"), 2);
  assert.equal(calendarTargetIndex(cells, 3, "Home"), 0);
  assert.equal(calendarTargetIndex(cells, 3, "End"), 6);
  assert.equal(calendarTargetIndex(cells, 13, "ArrowDown"), 13, "Sunday does not wrap into the next week");
});

test("a disabled multi-select cannot be cleared through its shortcut button", async () => {
  const source = await readFile(new URL("../src/shared/ui/ModelSelect.tsx", import.meta.url), "utf8");
  const clearButton = source.slice(source.indexOf("className={styles.clear}"), source.indexOf("><Icon icon={windowCloseIcon}", source.indexOf("className={styles.clear}")));
  assert.match(clearButton, /disabled={disabled}/, "disabled select must disable the clear shortcut");
});

test("the model menu sits above its host popover, not underneath it", async () => {
  const read = (path) => readFile(new URL(path, import.meta.url), "utf8");
  const zIndex = (source, selector) => {
    const start = source.indexOf(`${selector} {`);
    assert.notEqual(start, -1, `${selector} must exist`);
    const end = source.indexOf("}", start);
    const value = source.slice(start, end).match(/z-index:\s*(\d+)/)?.[1];
    assert.ok(value, `${selector} must declare a z-index`);
    return Number(value);
  };

  const [modelSelect, timeRange] = await Promise.all([
    read("../src/shared/ui/ModelSelect.module.scss"),
    read("../src/features/home/overview/OverviewTimeRangeFilter.module.scss"),
  ]);

  assert.ok(zIndex(modelSelect, ".menu") > zIndex(timeRange, ".popover"));
  assert.ok(zIndex(modelSelect, ".menu") < 15_000, "global tooltips and notifications stay above the picker");
});

test("the pending update check compares the installed version with the requested target", async () => {
  const { pendingUpdateOutcome } = await loader.load("/src/features/settings/pendingUpdate.ts");

  assert.deepEqual(pendingUpdateOutcome("1.6.16", "1.6.17", "1.6.17"), {
    from: "1.6.16",
    to: "1.6.17",
    actual: "1.6.17",
    applied: true,
  });
  assert.equal(pendingUpdateOutcome("1.6.16", "1.6.17", "1.6.16").applied, false);
  assert.equal(pendingUpdateOutcome("1.6.16", "1.6.17", "1.6.18").applied, false, "another version is not silently claimed as the target");
});
