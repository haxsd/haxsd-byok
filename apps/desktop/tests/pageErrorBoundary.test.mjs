// 页面错误边界的落点（17/P3-8）。
//
// 原来只有一层边界套在整个 keep-alive 容器外面：任何一页渲染出错，容器连同其它页面的
// 缓存状态一起被卸掉。修法是 keepalive-for-react 的 `errorElement`——它渲染在每个缓存
// 节点内部，错误只重建出错的那一页。SSR 不支持错误边界，所以这里测两件能测的事：
// 边界自身的契约（复位键、兜底界面），以及接线（errorElement 传进去、缓存节点里只手动复位）。
//
// 复核（第 33 批）指出：缓存节点内的复位键取 `_cacheKey` 后在节点存活期间恒定，"切走再切回
// 自动重试"根本不可达。已改成**只支持手动复位**，这里钉住这个决定，别让 `_cacheKey` 复活。

import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
// 组件外壳的 SCSS 先单独过一遍：SSR 模块加载器在把它当依赖处理时解析不了 `@use … as type`
// （Controls.module.scss），先加载一次就好（appCrashFallback.test.mjs 同样处理）。
await loader.load("/src/shared/ui/Controls.module.scss");
await loader.load("/src/shared/ui/EmptyState.module.scss");
installBrowserDoubles();
test.after(() => loader.close());

const boundary = () => loader.load("/src/shell/PageErrorBoundary.tsx");

test("a caught error renders the fallback with the raw message and a retry", async () => {
  const { PageErrorBoundary } = await boundary();
  const error = new Error("Cannot read properties of undefined (reading 'x')");

  assert.deepEqual(PageErrorBoundary.getDerivedStateFromError(error), { error });

  const markup = PageErrorBoundary.prototype.render.call({
    props: { children: null, resetKey: "/settings" },
    state: { error },
  });
  const html = (await import("react-dom/server")).renderToStaticMarkup(markup);
  assert.match(html, /这个页面出错了/);
  assert.match(html, /Cannot read properties of undefined/);
  assert.match(html, /重新加载这个页面/);
});

test("a page with no error renders its children untouched", async () => {
  const { PageErrorBoundary } = await boundary();
  const rendered = PageErrorBoundary.prototype.render.call({
    props: { children: "页面内容", resetKey: "/calls" },
    state: { error: null },
  });
  assert.equal(rendered, "页面内容");
});

test("changing the reset key clears a stuck error", async () => {
  const { PageErrorBoundary } = await boundary();
  const cleared = [];
  const instance = {
    props: { resetKey: "/calls" },
    state: { error: new Error("boom") },
    setState(patch) { cleared.push(patch); },
  };

  PageErrorBoundary.prototype.componentDidUpdate.call(instance, { resetKey: "/settings" });
  assert.deepEqual(cleared, [{ error: null }], "换页要自动重试，坏页面不能一直卡着");

  cleared.length = 0;
  PageErrorBoundary.prototype.componentDidUpdate.call(instance, { resetKey: "/calls" });
  assert.deepEqual(cleared, [], "同一页重渲染不该反复重置");
});

test("the boundary sits inside the keep-alive cache node and resets manually only", async () => {
  const layout = await readFile(new URL("../src/shell/AppLayout.tsx", import.meta.url), "utf8");
  const boundarySource = await readFile(new URL("../src/shell/PageErrorBoundary.tsx", import.meta.url), "utf8");
  const router = await readFile(new URL("../node_modules/keepalive-for-react/dist/index.d.ts", import.meta.url), "utf8");

  // 库把 errorElement 渲染在缓存节点内部（每个 cacheKey 一份），这是这条修法成立的前提。
  assert.match(router, /errorElement\?: ComponentType</);
  assert.match(layout, /errorElement=\{PageCacheErrorBoundary\}/, "缓存节点内部要有边界");

  // 外壳那层仍按路由复位（它套在容器外面，切页时键真的会变）。
  assert.match(layout, /<PageErrorBoundary resetKey=\{location\.pathname\}>/);

  // 缓存节点内只手动复位：`_cacheKey` 在节点存活期间恒定，拿它当复位键等于永远不触发，
  // 而"失败即自动复位"会让确定性渲染错误无限重放。
  assert.doesNotMatch(boundarySource, /useKeepAliveContext/, "_cacheKey 复位键不可达，已删掉");
  // 只查代码：注释里解释"为什么不用 _cacheKey"是应该留着的。
  const code = boundarySource.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/[^\n]*/g, "");
  assert.doesNotMatch(code, /_cacheKey/, "代码里不该再有缓存键复位");
  assert.match(boundarySource, /resetKey\?: string/, "复位键对缓存节点内的边界是可选的");
  assert.match(boundarySource, /<PageErrorBoundary>\{children\}<\/PageErrorBoundary>/, "缓存节点内的边界不传复位键");
  assert.match(boundarySource, /重新加载这个页面/, "手动复位按钮要留着");
  assert.match(boundarySource, /复位\*\*只支持手动\*\*/, "注释要写清为什么只能手动");
});
