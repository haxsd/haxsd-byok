// 命令面板的默认列表与调用行文字（17/P3-5、17/P3-6）。
//
// P3-5：空查询整体切前 12 条，「页面」7 条 + 「接入模块」3 条就把名额吃满，注释里写着
// "只给页面、动作和模型"，实际永远列不到模型。
// P3-6：调用行的 hint 直接印数据库状态（completed），绕开全仓统一的 callStatusLabel。

import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const palette = async () => {
  const { commandPaletteCommands } = await loader.load("/src/shell/commandPaletteCommands.ts");
  const matches = await loader.load("/src/shell/commandPaletteMatches.ts");
  const calls = [
    { call_id: "call-1", display_name: "第一次调用", model_id: "mock-gpt", conversation_id: "conv-1", created_at_ms: Date.UTC(2026, 7, 27, 8, 0, 0), status: "completed" },
    { call_id: "call-2", display_name: "第二次调用", model_id: "mock-gpt", conversation_id: "conv-2", created_at_ms: Date.UTC(2026, 7, 27, 8, 1, 0), status: "failed" },
  ];
  const models = [{ model_hash: "mock-gpt", model_id: "gpt-5.2", display_name: "GPT-5.2", base_url: "https://api.openai.com" }];
  const commands = commandPaletteCommands({
    calls,
    models,
    locale: "zh-CN",
    theme: "midnight",
    navigate: () => {},
    close: () => {},
  });
  return { commands, ...matches, calls };
};

test("the empty-query list keeps room for every group instead of filling up with pages", async () => {
  const { commands, emptyQueryMatches, EMPTY_QUERY_LIMIT } = await palette();

  const rows = emptyQueryMatches(commands);
  const groups = new Set(rows.map((command) => command.group));

  assert.ok(rows.length <= EMPTY_QUERY_LIMIT, "不越过总限额");
  assert.ok(groups.has("模型"), "模型组必须能出现在默认列表里（这正是 P3-5 的毛病）");
  assert.ok(groups.has("页面"));
  assert.ok(groups.has("动作"));
  assert.ok(
    rows.some((command) => command.id.startsWith("model-")),
    "至少要有一条模型命令，而不是被页面塞满",
  );
  assert.equal(
    rows.some((command) => command.id.startsWith("call-")),
    false,
    "调用记录可能有几百条，默认列表不列",
  );

  // 渲染按相邻项合并分组标题：同一个组必须连着出现。
  const seen = [];
  for (const command of rows) {
    if (seen.at(-1) !== command.group) seen.push(command.group);
  }
  assert.equal(new Set(seen).size, seen.length, "同一组的命令不能被别的组隔开");
});

test("a query still ranks by score and keeps the fuzzy behaviour", async () => {
  const { commands, paletteMatches } = await palette();

  const hit = paletteMatches(commands, "call-1");
  assert.equal(hit[0]?.id, "call-call-1", "按 ID 搜索要命中那一条调用");

  const fuzzy = paletteMatches(commands, "gpt5");
  assert.ok(fuzzy.length > 0, "连续字符命中仍然有效");
});

test("the call row shows the shared status label, not the database value", async () => {
  const { commands, calls } = await palette();
  const { callStatusLabel } = await loader.load("/src/shared/utils/callStatus.ts");

  const completed = commands.find((command) => command.id === `call-${calls[0].call_id}`);
  const failed = commands.find((command) => command.id === `call-${calls[1].call_id}`);

  assert.match(completed.hint, new RegExp(callStatusLabel("completed")));
  assert.doesNotMatch(completed.hint, /completed/, "中文界面上不该出现数据库里的英文状态");
  assert.match(failed.hint, new RegExp(callStatusLabel("failed")));
  assert.doesNotMatch(failed.hint, /· failed$/);
});
