// Behavior tests for the overview metric tiles' copy (features/home/metrics).
//
// 费用那一格以前会出现自相矛盾的组合：只要存在未定价用量，标题就写「已定价小计」，
// 而一个模型都没定价时数值又写「未定价」——标题在说有小计，数值在说没有小计。
// 这里用真实渲染把每个分支的标题、数值、说明一起钉住：
//
// 1. 标题与数值不得互相打脸（「已定价小计」这类半口径措辞不再出现）；
// 2. 有未定价用量时，卡片上仍要能看到有多少模型、多少 Token 没算进金额；
// 3. 给不出数字时说明原因，而不是挂一个 0 或临时换一个口径。
//
// 断言只看卡片渲染出的文字，不碰类名；币种跟随界面语言，这里固定成反馈里的中文界面。
//
// Harness note: the SCSS module is warmed up *before* the browser doubles are
// installed — see cursorToolsSettings.test.mjs.

import assert from "node:assert/strict";
import test from "node:test";
import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "vite";
import { desktopRoot, installBrowserDoubles } from "./harness.mjs";

/**
 * 服务端渲染需要的 `getServerSnapshot` 补丁。
 *
 * `renderToStaticMarkup` 要求 `useSyncExternalStore` 带第三个参数，而应用只在 WebView
 * 里跑，两个 store 都没写；缺了它 React 会直接放弃整次服务端渲染。这里只在这两个
 * store 模块的加载期补上「服务端快照 = 客户端快照」：快照本身、组件代码都是真的，
 * 补的只有这个参数。store 以后换写法，渲染会以 React 的原始报错直接失败。
 */
const serverSnapshotPlugin = {
  name: "test-server-snapshot",
  enforce: "pre",
  transform(source, id) {
    const snapshotHooks = ["/src/shared/store/appStore.ts", "/src/i18n/store.ts"];
    const imported = 'import { useSyncExternalStore } from "react";';
    if (!snapshotHooks.some((path) => id.split("?")[0].endsWith(path)) || !source.includes(imported)) return null;
    return source.replace(
      imported,
      'import { useSyncExternalStore as __ssrUseSyncExternalStore } from "react";\n' +
      "const useSyncExternalStore = (subscribe, getSnapshot, getServerSnapshot = getSnapshot) =>\n" +
      "  __ssrUseSyncExternalStore(subscribe, getSnapshot, getServerSnapshot);",
    );
  },
};

// harness 同款的模块加载器，外加上面这个补丁（harness.startModuleLoader 不接受插件）。
const server = await createServer({
  configFile: false,
  root: desktopRoot,
  appType: "custom",
  logLevel: "error",
  plugins: [serverSnapshotPlugin],
  server: { middlewareMode: true, hmr: false, ws: false },
});
const load = (modulePath) => server.ssrLoadModule(modulePath);
await load("/src/shared/ui/Controls.module.scss");
installBrowserDoubles();
test.after(() => server.close());

// 默认语言快照是 en-US（美元）；反馈里的界面是简体中文，这里把渲染固定成同一套。
const { setLocalePreference } = await load("/src/i18n/store.ts");
setLocalePreference("zh-CN");

const DATA = {
  llmCalls: 30,
  successfulCalls: 28,
  failedCalls: 2,
  tokenUsage: 9_000_000,
  promptTokens: 8_000_000,
  cacheReadTokens: 1_000_000,
  cacheWriteTokens: 500_000,
};

const STATS = [
  { startMs: 0, totalTokens: 3_000_000, inputTokens: 2_000_000, cacheReadTokens: 500_000, cacheWriteTokens: 200_000, outputTokens: 300_000, cacheHitRate: 0.2, cost: 0.5 },
  { startMs: 3_600_000, totalTokens: 6_000_000, inputTokens: 4_000_000, cacheReadTokens: 500_000, cacheWriteTokens: 300_000, outputTokens: 1_200_000, cacheHitRate: 0.11, cost: 1.18 },
];

function modelRow(label, tokens, cost) {
  return { key: `model:${label}`, label, modelHash: null, tokens, calls: 10, cost };
}

/** 和真实估算器一样，汇总值只从模型行推导，不另外喂一份可能对不上的总数。 */
function pricingEstimate(rows, patch = {}) {
  return {
    input: 0,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
    total: rows.reduce((sum, row) => sum + (row.cost ?? 0), 0),
    totalTokens: rows.reduce((sum, row) => sum + row.tokens, 0),
    unpricedTokens: rows.reduce((sum, row) => sum + (row.cost === null ? row.tokens : 0), 0),
    models: rows,
    hours: [],
    ...patch,
  };
}

/** 一张卡片的可见文案：标签剥掉、空白压平。 */
function tileText(markup) {
  return markup.replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();
}

/**
 * 费用那一格的文案。
 *
 * 指标区固定渲染四张卡片（缓存命中率、LLM 调用、Token 消耗、费用），每张一个
 * `<article>`，所以第四张就是费用卡；这里只取它的文字，不碰类名。
 */
function costTileCopy(markup) {
  const tiles = markup.split(/<article\b[^>]*>/).slice(1);
  assert.equal(tiles.length, 4, "指标区应当渲染四张卡片");
  return tileText(tiles[3]);
}

/** 渲染指标区，返回费用卡的文字。 */
async function renderCostTile(overrides = {}) {
  const [{ HomeMetrics }, { TooltipProvider }] = await Promise.all([
    load("/src/features/home/metrics/HomeMetrics.tsx"),
    load("/src/shared/ui/Tooltip.tsx"),
  ]);
  const markup = renderToStaticMarkup(h(TooltipProvider, null, h(HomeMetrics, {
    data: DATA,
    stats: STATS,
    callCounts: [12, 18],
    estimate: null,
    pricingFailure: null,
    refreshVersion: 0,
    ...overrides,
  })));
  return costTileCopy(markup);
}

/**
 * 标题与数值不得互相打脸。
 *
 * 原始组合：标题「已定价小计」+ 数值「未定价」——一个说有小计，一个说没有。
 */
function assertTitleAgreesWithValue(text) {
  const titleClaimsSubtotal = /已定价小计/.test(text);
  const valueSaysNoAmount = /未定价|估算不可用/.test(text);
  assert.ok(!(titleClaimsSubtotal && valueSaysNoAmount), `标题与数值互相打脸：${text}`);
}

const UNPRICED_ONLY = [modelRow("GPT-X", 4_000_000, null), modelRow("Claude-Y", 3_000_000, null)];

test("一个模型都没定价时，标题与数值都只说未定价，不声称小计", async () => {
  const text = await renderCostTile({ estimate: pricingEstimate(UNPRICED_ONLY) });
  assertTitleAgreesWithValue(text);
  assert.doesNotMatch(text, /已定价小计/, "半口径的「已定价小计」不能再出现在卡片上");
  assert.doesNotMatch(text, /估算不可用/, "没填单价不等于功能坏了，不要说估算不可用");
  assert.match(text, /^价值估算 未定价/, "标题是全口径的价值估算，数值直接说未定价");
  assert.match(text, /2 个模型未定价，7M Token 未计入金额/, "未定价用量的说明仍要在卡片上");
});

test("部分模型未定价时，数值是已定价部分的钱，标题仍是全口径的价值估算", async () => {
  const text = await renderCostTile({
    estimate: pricingEstimate([modelRow("GPT-X", 4_000_000, 1.68), modelRow("Claude-Y", 3_000_000, null)]),
  });
  assertTitleAgreesWithValue(text);
  assert.doesNotMatch(text, /已定价小计/);
  assert.match(text, /^价值估算 ¥1\.68/, "标题与数值说的都是已定价部分的金额");
  assert.match(text, /1 个模型未定价，3M Token 未计入金额/, "未定价用量的说明仍要在卡片上");
});

test("全部模型都已定价时，数值是完整估算", async () => {
  const text = await renderCostTile({
    estimate: pricingEstimate(
      [modelRow("GPT-X", 4_000_000, 1.56), modelRow("Claude-Y", 3_000_000, 0.12)],
      { cacheRead: 0.1, cacheWrite: 0.02 },
    ),
  });
  assertTitleAgreesWithValue(text);
  assert.match(text, /^价值估算 ¥1\.68/);
  assert.match(text, /缓存读写 ¥0\.12/);
  assert.doesNotMatch(text, /未定价/, "没有未定价用量时不要提未定价");
});

test("已定价模型单价为 0 时，数值就是真实的 0，说明里照样写清未定价用量", async () => {
  const text = await renderCostTile({
    estimate: pricingEstimate([modelRow("Free-Model", 1_000_000, 0), modelRow("Claude-Y", 3_000_000, null)]),
  });
  assertTitleAgreesWithValue(text);
  assert.match(text, /^价值估算 ¥0\.00/, "免费但已定价的模型是真 0，不能改写成未定价");
  assert.match(text, /1 个模型未定价，3M Token 未计入金额/);
});

test("估算还没算出来时，卡片说明正在读取，而不是显示 0", async () => {
  const text = await renderCostTile();
  assertTitleAgreesWithValue(text);
  assert.match(text, /^价值估算 计算中…/);
  assert.match(text, /正在按模型读取用量/);
  assert.doesNotMatch(text, /¥0\.00/, "还没算完不能显示成 0");
});

for (const [failure, reason] of [
  ["range", /所选范围超过 660 天，未生成费用估算/],
  ["read", /用量读取失败，未生成费用估算/],
]) {
  test(`取数失败（${failure}）时标题不再声称小计，并写清失败原因`, async () => {
    const text = await renderCostTile({ pricingFailure: failure });
    assertTitleAgreesWithValue(text);
    assert.match(text, /^价值估算 估算不可用/);
    assert.match(text, reason);
    assert.doesNotMatch(text, /已定价小计/);
  });
}
