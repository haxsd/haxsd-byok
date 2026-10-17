// 启动路径上的主题偏好读取，以及 index.html 的预涂底色。
//
// `appStore.ts` 的 savedTheme() 在**模块作用域**执行，是启动路径上唯一没有保护的
// localStorage 读取：存储被禁用/被策略拦下时模块初始化就抛错，而那时错误边界还没挂上
// （它在 index.tsx 的 render 里），用户看到的是一片空白。
//
// 预涂底色（index.html 内联脚本）只认 default-light/default-dark，默认主题 midnight
// 在启动瞬间被当成 default-dark，窗口会闪一下。

import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

test("a storage that throws cannot take the whole store down at import time", async () => {
  const original = globalThis.localStorage;
  globalThis.localStorage = {
    getItem: () => { throw new Error("storage is disabled by policy"); },
    setItem: () => { throw new Error("storage is disabled by policy"); },
    removeItem: () => { throw new Error("storage is disabled by policy"); },
  };
  try {
    loader.invalidateAll();
    const { appStore } = await loader.load("/src/shared/store/appStore.ts");
    const { defaultThemeId } = await loader.load("/src/shared/theme/theme.ts");
    assert.equal(appStore.getSnapshot().theme, defaultThemeId, "读不到偏好就用默认主题，不能让模块初始化失败");
  } finally {
    globalThis.localStorage = original;
    loader.invalidateAll();
  }
});

test("the theme no longer starts from the origin-bound legacy key", async () => {
  // 主题现在由服务端说了算（shared/store/uiPreferences.ts）：启动先按默认主题渲染，服务端的
  // 值到了再应用。旧 localStorage 里的值只在迁移时读一次（uiPreferences.test.mjs 覆盖）。
  globalThis.localStorage.setItem("haxsd-byok.theme", "default-light");
  try {
    loader.invalidateAll();
    const { appStore } = await loader.load("/src/shared/store/appStore.ts");
    const { defaultThemeId } = await loader.load("/src/shared/theme/theme.ts");
    assert.equal(appStore.getSnapshot().theme, defaultThemeId, "不再从那份绑在管理端口 origin 上的值启动");
  } finally {
    globalThis.localStorage.removeItem("haxsd-byok.theme");
    loader.invalidateAll();
  }
});

test("every theme the app offers has a pre-paint colour in index.html", async () => {
  const { themeOptions, defaultThemeId } = await loader.load("/src/shared/theme/theme.ts");
  const html = await readFile(new URL("../index.html", import.meta.url), "utf8");

  for (const option of themeOptions) {
    // 必须是 `<id>: { background: "#……"` 这样的表项（键带不带引号都行），光是文案里
    // 出现 id 不算——曾经的写法里 id 只出现在判断和兜底分支里，根本没有这个主题的底色。
    assert.match(
      html,
      new RegExp(`["']?${option.id}["']?\\s*:\\s*\\{\\s*background:\\s*"#[0-9a-fA-F]{3,8}"`),
      `主题 ${option.id} 必须在预涂脚本里有一条底色`,
    );
  }
  assert.match(
    html,
    new RegExp(`stored\\s*:\\s*"${defaultThemeId}"`),
    `兜底底色必须是默认主题 ${defaultThemeId}`,
  );
});
