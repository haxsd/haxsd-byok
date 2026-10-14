import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());
const navigation = () => loader.load("/src/features/settings/settingsNavigation.ts");

test("default settings show only general preferences, not updates or diagnostics", async () => {
  const { settingsSections, visibleSettingsSections } = await navigation();
  const shown = visibleSettingsSections(settingsSections(false), null, "");
  assert.deepEqual(shown.map(section => section.id), ["general"]);
  assert.deepEqual(shown[0].items.map(item => item.id), ["appearance", "app"]);
});

test("selecting a category replaces content instead of appending all settings", async () => {
  const { settingsSections, visibleSettingsSections } = await navigation();
  const shown = visibleSettingsSections(settingsSections(false), "network", "");
  assert.deepEqual(shown.flatMap(section => section.items.map(item => item.id)), ["proxy"]);
});

test("search finds a concrete setting outside the selected category", async () => {
  const { settingsSections, visibleSettingsSections } = await navigation();
  const shown = visibleSettingsSections(settingsSections(false), "general", "  定价  ");
  assert.deepEqual(shown.map(section => section.id), ["usage"]);
  assert.deepEqual(shown[0].items.map(item => item.id), ["pricing"]);
});

test("search accepts case-insensitive technical terms and multiple keywords", async () => {
  const { settingsSections, visibleSettingsSections } = await navigation();
  const shown = visibleSettingsSections(settingsSections(false), "general", "PROXY 网络");
  assert.deepEqual(shown.flatMap(section => section.items.map(item => item.id)), ["proxy"]);
  assert.deepEqual(visibleSettingsSections(settingsSections(false), "general", "unmatched-setting"), []);
});

test("destination search matches the same words in either order", async () => {
  const { settingsDestinations } = await navigation();
  assert.deepEqual(settingsDestinations("模型 导入").map(item => item.path), ["/models"]);
  assert.deepEqual(settingsDestinations(" 导入   模型 ").map(item => item.path), ["/models"]);
  assert.deepEqual(settingsDestinations("提交 COMMIT").map(item => item.path), ["/harness/cursor?section=tools"]);
  assert.deepEqual(settingsDestinations("模型 提交").map(item => item.path), ["/harness/cursor?section=tools"]);
  assert.deepEqual(settingsDestinations("  "), []);
  assert.deepEqual(settingsDestinations("模型 unmatched-setting"), []);
});

test("advanced port controls stay hidden until developer mode is enabled", async () => {
  const { settingsSections, visibleSettingsSections } = await navigation();
  const normal = visibleSettingsSections(settingsSections(false), "advanced", "");
  const developer = visibleSettingsSections(settingsSections(true), "advanced", "");
  assert.equal(normal[0].items.some(item => item.id === "ports"), false);
  assert.equal(developer[0].items.some(item => item.id === "ports"), true);
  assert.deepEqual(visibleSettingsSections(settingsSections(false), "obsolete", "").map(section => section.id), ["general"]);
});
