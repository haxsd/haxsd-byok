// 界面偏好落服务端（17/P2-1 + 01/P2-3）。
//
// 根因：偏好只存在 webview 的 localStorage 里，而 localStorage 按 origin 隔离、origin 里带着
// 管理端口——端口一变偏好全部读不到。现在服务端是唯一权威，localStorage 只剩一次性迁移。
// 这一组用例钉住三件事：读回落 UI、写回被调用、旧数据只迁移一次（且服务端没确认前不许删旧键）。

import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { API_ROOT, installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { installAppBackend } from "./appBackend.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const DESKTOP = `${API_ROOT}/settings/desktop`;
const PREFERENCE_FIELDS = ["theme", "locale", "tutorial_read", "pending_update", "devin_host_path", "devin_install_root", "disclosure"];

/**
 * 一个只认 /settings/desktop 的假服务端。
 *
 * `echo` 决定 PUT 的回显：false 模拟"接口还没落地这些字段"——服务端忽略未知字段、照样 200。
 */
function installDesktopSettings(initial, { echo = true } = {}) {
  const { backend } = installAppBackend();
  let stored = { silent_start: false, show_dock_icon: true, developer_mode: false, ...initial };
  backend.route("GET", DESKTOP, () => ({ body: { ...stored } }));
  backend.route("PUT", DESKTOP, (request) => {
    const patch = request.body ?? {};
    if (echo) {
      stored = { ...stored, ...patch };
      if (patch.disclosure) stored.disclosure = { ...(initial.disclosure ?? {}), ...patch.disclosure };
    }
    return { body: { ...stored } };
  });
  return { backend, stored: () => stored };
}

async function loadPreferences() {
  loader.invalidateAll();
  return loader.load("/src/shared/store/uiPreferences.ts");
}

const storage = () => globalThis.localStorage;
function seedLegacy(values) {
  for (const [key, value] of Object.entries(values)) storage().setItem(key, value);
}
function legacyKeys() {
  const keys = [];
  for (let index = 0; index < storage().length; index += 1) keys.push(storage().key(index));
  return keys.filter((key) => key.startsWith("haxsd-byok"));
}

test("the server is authoritative: its values win and the old keys are dropped", async () => {
  installDesktopSettings({
    theme: "default-light",
    locale: "en-US",
    tutorial_read: true,
    devin_host_path: "C:\\Users\\me\\AppData\\Roaming\\Devin\\User\\settings.json",
    devin_install_root: "D:\\Devin",
    disclosure: { advanced: true },
  });
  seedLegacy({ "haxsd-byok.theme": "default-dark", "haxsd-byok:tutorial-read": "false" });
  const { loadUiPreferences } = await loadPreferences();

  const preferences = await loadUiPreferences();

  assert.equal(preferences.theme, "default-light", "服务端的主题优先于旧 localStorage");
  assert.equal(preferences.locale, "en-US");
  assert.equal(preferences.tutorialRead, true);
  assert.equal(preferences.devinHostPath, "C:\\Users\\me\\AppData\\Roaming\\Devin\\User\\settings.json");
  assert.equal(preferences.devinInstallRoot, "D:\\Devin");
  assert.deepEqual(preferences.disclosure, { advanced: true });
  assert.deepEqual(legacyKeys(), [], "服务端接管之后旧键没用了，要删掉");
});

test("with nothing on the server the old values are migrated once, then the keys are gone", async () => {
  const settings = installDesktopSettings({});
  seedLegacy({
    "haxsd-byok.theme": "default-light",
    "haxsd-byok.locale": "zh-CN",
    "haxsd-byok:tutorial-read": "true",
    "haxsd-byok.pending-update": JSON.stringify({ from: "1.4.0", to: "1.5.0" }),
    "haxsd-byok.devin.host-path": "D:\\Devin\\extension.js",
    "haxsd-byok:disclosure:advanced": "open",
    "haxsd-byok:disclosure:proxy": "closed",
  });
  const { loadUiPreferences } = await loadPreferences();

  const preferences = await loadUiPreferences();

  assert.deepEqual(preferences.theme, "default-light", "服务端还没有值，先用旧的");
  assert.deepEqual(preferences.locale, "zh-CN");
  assert.equal(preferences.tutorialRead, true);
  assert.deepEqual(preferences.pendingUpdate, { from: "1.4.0", to: "1.5.0" });
  assert.equal(preferences.devinHostPath, "D:\\Devin\\extension.js");
  assert.deepEqual(preferences.disclosure, { advanced: true, proxy: false }, "折叠状态也要搬");

  const uploads = settings.backend.requestsFor("PUT", DESKTOP);
  assert.equal(uploads.length, 1, "旧数据只上传一次");
  assert.equal(uploads[0].body.theme, "default-light");
  assert.equal(uploads[0].body.locale, "zh-CN");
  assert.equal(uploads[0].body.tutorial_read, true);
  assert.deepEqual(uploads[0].body.pending_update, { from: "1.4.0", to: "1.5.0" });
  assert.deepEqual(uploads[0].body.disclosure, { advanced: true, proxy: false });
  assert.deepEqual(legacyKeys(), [], "服务端回显确认之后旧键才可以删");

  // 下一次启动：服务端已有值，不能再上传一次（这就是"只迁移一次"）。
  const second = await loadPreferences();
  const again = await second.loadUiPreferences();
  assert.equal(again.theme, "default-light");
  assert.equal(settings.backend.requestsFor("PUT", DESKTOP).length, 1, "第二次启动不该再 PUT");
  assert.deepEqual(legacyKeys(), [], "旧键不会自己回来");
});

test("a server that does not echo the fields keeps the old keys instead of losing them", async () => {
  const settings = installDesktopSettings({}, { echo: false });
  seedLegacy({ "haxsd-byok.theme": "default-light" });
  const { loadUiPreferences } = await loadPreferences();

  const preferences = await loadUiPreferences();

  assert.equal(preferences.theme, "default-light", "接口没落地时旧值照旧可用");
  assert.equal(settings.backend.requestsFor("PUT", DESKTOP).length, 1, "还是要试着上传");
  assert.deepEqual(legacyKeys(), ["haxsd-byok.theme"], "服务端没确认收到就不能删旧键（否则偏好被弄丢）");
});

test("changing a preference applies locally and writes it back", async () => {
  const settings = installDesktopSettings({ theme: "midnight" });
  const { saveUiPreferences, getUiPreferences } = await loadPreferences();

  const confirmed = await saveUiPreferences({ theme: "default-light" });

  assert.equal(confirmed, true);
  assert.equal(getUiPreferences().theme, "default-light", "本地立刻生效");
  const writes = settings.backend.requestsFor("PUT", DESKTOP);
  assert.equal(writes.length, 1);
  assert.deepEqual(writes[0].body, { theme: "default-light" }, "只提交改动的那一个字段");
  assert.equal(settings.stored().theme, "default-light");
});

test("a preference the server cannot store is reported as unconfirmed", async () => {
  installDesktopSettings({}, { echo: false });
  const { saveUiPreferences, getUiPreferences } = await loadPreferences();

  const confirmed = await saveUiPreferences({ theme: "default-dark" });

  assert.equal(confirmed, false, "服务端没回显就不算保存成功");
  assert.equal(getUiPreferences().theme, "default-dark", "这一次会话里仍然生效");
});

test("the plan picks server, legacy or defaults without guessing", async () => {
  const { planPreferenceBoot } = await loadPreferences();
  const legacy = { theme: "default-light" };

  const fromServer = planPreferenceBoot({ silent_start: false, show_dock_icon: true, developer_mode: false, theme: "midnight" }, legacy);
  assert.deepEqual(fromServer, { preferences: { theme: "midnight" }, upload: null, clearLegacy: true });

  const migrate = planPreferenceBoot({ silent_start: false, show_dock_icon: true, developer_mode: false }, legacy);
  assert.deepEqual(migrate, { preferences: legacy, upload: legacy, clearLegacy: false });

  const nothing = planPreferenceBoot({ silent_start: false, show_dock_icon: true, developer_mode: false }, {});
  assert.deepEqual(nothing, { preferences: {}, upload: null, clearLegacy: false });

  // 字段出现但是 null = 用户显式清空过：也算服务端接管，不能拿旧值去覆盖。
  const cleared = planPreferenceBoot({ silent_start: false, show_dock_icon: true, developer_mode: false, theme: null }, legacy);
  assert.deepEqual(cleared, { preferences: {}, upload: null, clearLegacy: true });
});

test("unknown or malformed server values are treated as unset, not guessed", async () => {
  const { preferencesFromSettings, coversPreferences, preferencePatch } = await loadPreferences();

  assert.deepEqual(preferencesFromSettings({
    silent_start: false, show_dock_icon: true, developer_mode: false,
    theme: "solarized", locale: "fr-FR", tutorial_read: "yes", pending_update: { from: "1" },
    devin_host_path: "", devin_install_root: 42, disclosure: { good: true, bad: "yes" },
  }), { disclosure: { good: true } });

  assert.equal(coversPreferences({ theme: "midnight" }, { theme: "midnight" }), true);
  assert.equal(coversPreferences({ theme: "default-dark" }, { theme: "midnight" }), false);
  assert.equal(coversPreferences({}, { theme: "midnight" }), false);
  assert.equal(coversPreferences({ disclosure: { a: true, b: false } }, { disclosure: { a: true } }), true, "折叠状态按上传的键核对");
  assert.equal(coversPreferences({ disclosure: { a: false } }, { disclosure: { a: true } }), false);

  assert.deepEqual(preferencePatch({ tutorialRead: false }), { tutorial_read: false }, "false 也是值，不能被吞掉");
  assert.deepEqual(preferencePatch({ pendingUpdate: null }), { pending_update: null }, "null = 清空");
  assert.deepEqual(preferencePatch({ devinInstallRoot: "D:\\Devin" }), { devin_install_root: "D:\\Devin" });
  assert.deepEqual(preferencePatch({}), {});
});

test("the boot puts the server's theme and language on the UI", async () => {
  installDesktopSettings({ theme: "default-light", locale: "zh-CN", tutorial_read: true });
  loader.invalidateAll();
  const { bootstrapUiPreferences } = await loader.load("/src/shared/store/bootstrapPreferences.ts");
  const { initializeI18n } = await loader.load("/src/i18n/store.ts");
  const { appStore } = await loader.load("/src/shared/store/appStore.ts");
  initializeI18n();

  await bootstrapUiPreferences();

  assert.equal(appStore.getSnapshot().theme, "default-light", "主题要落到 store");
  assert.equal(globalThis.document.documentElement.dataset.theme, "default-light", "也要落到 DOM 上");
  assert.equal(globalThis.document.documentElement.lang, "zh-CN", "语言要落到 DOM 上");
  assert.equal(appStore.getSnapshot().error, null, "读偏好不算失败");
});

test("changing the theme or language from the UI writes it back", async () => {
  const settings = installDesktopSettings({});
  loader.invalidateAll();
  const { appStore } = await loader.load("/src/shared/store/appStore.ts");
  const { setLocalePreference } = await loader.load("/src/i18n/store.ts");

  appStore.selectTheme("default-light");
  setLocalePreference("en-US");
  await new Promise((resolve) => setImmediate(resolve));

  const writes = settings.backend.requestsFor("PUT", DESKTOP);
  assert.deepEqual(writes.map((write) => write.body), [{ theme: "default-light" }, { locale: "en-US" }]);
  assert.equal(settings.stored().theme, "default-light");
  assert.equal(settings.stored().locale, "en-US");
  assert.equal(appStore.getSnapshot().theme, "default-light", "界面立刻反映");
});

test("no production module outside the migration reads or writes the origin-bound keys", async () => {
  const modules = [
    "../src/shared/store/appStore.ts",
    "../src/i18n/store.ts",
    "../src/shared/ui/TitledCard.tsx",
    "../src/shell/AppLayout.tsx",
    "../src/features/settings/pendingUpdate.ts",
    "../src/features/devin/DevinSettingsPage.tsx",
  ];
  for (const module of modules) {
    const source = await readFile(new URL(module, import.meta.url), "utf8");
    // 只查真正的调用：注释里提一句 localStorage 不算（要说清为什么不这么做）。
    assert.doesNotMatch(source, /localStorage\s*\.\s*\w+/, `${module} 不该再调用 localStorage（那是这条 bug 的根因）`);
  }
  const migration = await readFile(new URL("../src/shared/store/uiPreferences.ts", import.meta.url), "utf8");
  assert.match(migration, /LEGACY_KEYS/, "迁移只允许发生在这一个模块里");
  assert.deepEqual(PREFERENCE_FIELDS.length, 7);
});
