// 界面外壳的兜底与「更多」菜单的键盘行为。
//
// 白屏那条是从真实无头 Chrome 里量出来的：`/api/models` 返回 [null] 后，
// 命令面板在渲染里抛错，React 卸掉整棵树，root 的子节点数为 0、导航与页头全部消失。
// SSR 不支持错误边界，所以这里断言兜底界面本身可用；"真的能接住"由浏览器探针证明。

import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
await loader.load("/src/shell/AppErrorBoundary.module.scss");
await loader.load("/src/shared/ui/Controls.module.scss");
installBrowserDoubles();
test.after(() => loader.close());

test("the crash fallback names what happened, keeps the raw error and offers recovery", async () => {
  const { AppCrashFallback } = await loader.load("/src/shell/AppErrorBoundary.tsx");
  const markup = renderToStaticMarkup(createElement(AppCrashFallback, {
    error: new Error("Cannot read properties of null (reading 'model_hash')"),
    onRetry: () => {},
  }));

  assert.match(markup, /这个页面出错了/);
  assert.match(markup, /错误原文：/, "the raw message must stay readable for the developer path");
  assert.match(markup, /Cannot read properties of null/);
  assert.match(markup, /重试/);
  assert.match(markup, /重新加载这个页面/);
});

test("without an error the boundary renders its children untouched", async () => {
  const { AppErrorBoundary } = await loader.load("/src/shell/AppErrorBoundary.tsx");
  const markup = renderToStaticMarkup(createElement(AppErrorBoundary, null, createElement("p", null, "页面内容")));
  assert.equal(markup, "<p>页面内容</p>");
});

test("menu focus starts at the first usable item and never lands on disabled or label rows", async () => {
  const { firstMenuItemIndex, nextMenuItemIndex } = await loader.load("/src/shared/ui/ActionMenu.tsx");
  const items = [
    { id: "caption", label: "说明", type: "text" },
    { id: "disabled", label: "删除", disabled: true, onSelect: () => {} },
    { id: "edit", label: "编辑", onSelect: () => {} },
    { id: "copy", label: "复制", onSelect: () => {} },
  ];

  assert.equal(firstMenuItemIndex(items), 2, "the first focusable item skips the caption and the disabled row");
  assert.equal(nextMenuItemIndex(items, 2, 1), 3);
  assert.equal(nextMenuItemIndex(items, 3, 1), 2, "the last item wraps to the first");
  assert.equal(nextMenuItemIndex(items, 2, -1), 3, "the first item wraps backwards to the last");
  assert.equal(nextMenuItemIndex(items, -1, 1), 2, "arrow-down on a fresh menu enters at the first item");
  assert.equal(nextMenuItemIndex([{ id: "caption", label: "说明", type: "text" }], -1, 1), -1, "a menu of captions has nothing to focus");
});

test("the empty call list asks for a first call only when there is no call at all", async () => {
  const { callListEmptyCopy } = await loader.load("/src/features/calls/CallTable.tsx");

  const firstRun = callListEmptyCopy(true);
  assert.equal(firstRun.title, "还没有调用记录");
  assert.doesNotMatch(firstRun.description, /筛选/, "a first-run user has no filter to clear");
  assert.match(firstRun.description, /接好模型后/);

  const filtered = callListEmptyCopy(false);
  assert.equal(filtered.title, "没有符合条件的调用");
  assert.match(filtered.description, /筛选/);
  assert.notEqual(firstRun.description, filtered.description, "the two empty states must not share one sentence");
});
