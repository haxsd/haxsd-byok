// 调用页的来源筛选与路由列。
//
// Claude 网关接进来以后，调用页必须能按 Claude 筛选，表格也必须给 Claude 自己的
// 标签——如果它落进通用 `BYOK`，Cursor/Codex/Devin/Claude 四条接入路径在列表里
// 看起来都是同一条。这里钉住筛选取值和路由标签这两个纯函数，页面直接消费它们。

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
installBrowserDoubles();
test.after(() => loader.close());

test("the calls page source filter lists Claude exactly once", async () => {
  const { callClientLabel, callClientSources } = await loader.load("/src/shared/utils/callStatus.ts");

  assert.deepEqual(
    [...callClientSources],
    ["cursor", "devin", "codex", "claude", "internal"],
    "CallsPage builds its source options from this list",
  );
  assert.equal(callClientLabel("claude"), "Claude", "the filter and the detail panel share one name");
});

test("a Claude call keeps its own route label instead of the generic BYOK", async () => {
  const { callRouteLabel } = await loader.load("/src/shared/utils/callStatus.ts");

  assert.equal(callRouteLabel({ client_source: "claude", route: "local_byok" }), "Claude · BYOK");
  assert.equal(callRouteLabel({ client_source: "codex", route: "local_byok" }), "Codex · BYOK");
  assert.equal(callRouteLabel({ client_source: "devin", route: "local_byok" }), "Devin · BYOK");
  assert.equal(callRouteLabel({ client_source: "cursor", route: "local_byok" }), "BYOK");
  assert.equal(callRouteLabel({ client_source: "cursor", route: "cursor_official" }), "官方");
});
