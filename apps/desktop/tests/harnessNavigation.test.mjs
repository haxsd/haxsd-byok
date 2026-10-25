// 接入模块导航的行为测试。
//
// 顶栏只留一个「接入」入口：客户端数量只影响接入页里的切换项，不再让顶栏变长。
// 这里锁定三件事——清单是唯一来源、聚合状态的算法、切换项真的按清单渲染。

import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter } from "react-router-dom";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

const loader = await startModuleLoader();
await loader.load("/src/shared/ui/Controls.module.scss");
installBrowserDoubles();
test.after(() => loader.close());

const idle = { cursorHarness: null, devinStatus: null, codexStatus: null, claudeStatus: null, claudeTakeover: null };
const state = (overrides) => ({ ...idle, ...overrides });

test("every client module is declared once, with its own path and status", async () => {
  const { harnessClients, harnessClientAt } = await loader.load("/src/features/harness/harnessClients.ts");

  assert.deepEqual(harnessClients.map((client) => client.id), ["cursor", "devin", "codex", "claude"]);
  assert.deepEqual(harnessClients.map((client) => client.path), ["/harness/cursor", "/harness/devin", "/harness/codex", "/harness/claude"]);
  for (const client of harnessClients) {
    assert.ok(client.label.length > 0, `${client.id} needs a label`);
    assert.ok(client.icon, `${client.id} needs an icon`);
    assert.equal(client.entry(idle).active, false, `${client.id} is not active before the first read`);
  }
  // 没读到状态就不算「在用本机网关」，也不能说成关着。
  assert.equal(harnessClients[0].entry(idle).status, null, "Cursor reports nothing before the first read");
  assert.equal(harnessClients[1].entry(idle).status, null, "Devin reports nothing before the first read");
  assert.equal(harnessClients[2].entry(idle).status.state, "checking", "Codex reports that it is still reading");
  assert.equal(harnessClients[3].entry(idle).status.state, "checking", "Claude reports that it is still reading");

  assert.equal(harnessClientAt("/harness/devin")?.id, "devin", "an exact client path resolves");
  assert.equal(harnessClientAt("/harness/codex/sessions")?.id, "codex", "a child path resolves to its client");
  assert.equal(harnessClientAt("/models"), null, "a page outside the module resolves to nothing");
});

test("the top entry counts active clients and takes the tone of the one that needs attention", async () => {
  const { harnessEntryStatus } = await loader.load("/src/features/harness/harnessClients.ts");

  const none = harnessEntryStatus(idle);
  assert.equal(none.state, "none-active");
  assert.equal(none.tone, "idle");

  const cursorOnly = harnessEntryStatus(state({
    cursorHarness: { mode: "local", settings_applied: true },
  }));
  assert.equal(cursorOnly.state, "1-active");
  assert.equal(cursorOnly.tone, "ok");
  assert.match(cursorOnly.label, /1\/4/);

  // Devin 保存了但端口还没起来：这一项是警告，聚合入口就该是警告色。
  const pending = harnessEntryStatus(state({
    cursorHarness: { mode: "local", settings_applied: true },
    devinStatus: { enabled: true, listening: false, ready: false },
  }));
  assert.equal(pending.tone, "warn");

  // Codex 配置冲突比警告更需要处理。
  const conflict = harnessEntryStatus(state({
    devinStatus: { enabled: true, listening: false, ready: false },
    codexStatus: { enabled: true, listening: true, port: 43120, configured: false, config_conflict: true, home: "x", calls: 0, last_error: null },
  }));
  assert.equal(conflict.tone, "bad");
  assert.equal(conflict.label, "未接入", "a conflict is not counted as an active client");
});

test("the page-level switch renders one entry per declared client with its own status", async () => {
  const { HarnessTabs } = await loader.load("/src/features/harness/HarnessTabs.tsx");
  const { TooltipProvider } = await loader.load("/src/shared/ui/Tooltip.tsx");

  const markup = renderToStaticMarkup(createElement(
    TooltipProvider,
    null,
    createElement(
      MemoryRouter,
      { initialEntries: ["/harness/devin"] },
      createElement(HarnessTabs, {
        state: state({
          cursorHarness: { mode: "local", settings_applied: true },
          devinStatus: { enabled: true, listening: true, ready: true },
        }),
      }),
    ),
  ));

  for (const [path, label] of [["/harness/cursor", "Cursor"], ["/harness/devin", "Devin"], ["/harness/codex", "Codex"], ["/harness/claude", "Claude"]]) {
    assert.match(markup, new RegExp(`href="${path}"`), `${path} is reachable from the switch`);
    assert.match(markup, new RegExp(`>${label}<`), `${label} keeps its own label`);
  }
  assert.match(markup, /已接管/, "Cursor reports its own takeover state");
  assert.match(markup, /运行中/, "Devin reports its own gateway state");
  assert.match(markup, /aria-current="page"[^>]*>[\s\S]*Devin/, "the current client is the marked one");
});

test("server errors are shown as readable text, never as the raw service sentence", async () => {
  const { clientErrorText } = await loader.load("/src/features/harness/harnessErrorText.ts");

  // 界面上出现过的真实原句：Devin 探测不到安装、路径不是设置文件、设置文件不可解析，
  // 以及 Codex 的应用前校验。
  assert.match(clientErrorText(new Error("configuration error: Devin installation not found; 42 locations were searched. Enter the installation directory in the installation path field")), /没有找到 Devin 安装/);
  assert.match(clientErrorText(new Error("configuration error: D:\\x is not a Devin settings file or data directory")), /不是 Devin 的设置文件或数据目录/);
  assert.match(clientErrorText(new Error("configuration error: Devin settings file is not valid JSONC: expected value")), /不是有效的 JSONC/);
  assert.match(clientErrorText(new Error("configuration error: restore the applied Codex configuration before changing gateway settings")), /先撤销接管/);
  assert.match(clientErrorText(new Error("configuration error: configure the actual context window for GPT")), /上下文窗口/);
  assert.match(clientErrorText(new Error("configuration error: Devin default model must be selected before enabling the gateway")), /默认模型/);
  assert.match(clientErrorText(new Error("configuration error: Devin default model UID not found: gone")), /默认模型映射/);
  // 写入 Devin 设置前的监听检查（server/src/devin/gateway.rs 的原句）：网关没在听、
  // 和网关在听但不是设置里那几个端口，是两件要分别照着做的事。
  assert.match(
    clientErrorText(new Error("configuration error: the Devin gateway is not listening yet; enable it and restart haxsd byok so it binds the configured ports before writing Devin settings")),
    /还没有开始监听/,
  );
  assert.match(
    clientErrorText(new Error("configuration error: the Devin gateway is listening on 41000 / 41001, not on the configured 42000 / 42001; restart haxsd byok so it binds the saved ports before writing Devin settings")),
    /端口与当前设置不一致/,
  );

  // 认不出的原因保留原句（去掉前缀），否则排查时什么都看不到。
  assert.equal(clientErrorText(new Error("configuration error: something new happened")), "something new happened");
  assert.equal(clientErrorText(new Error("io error: 拒绝访问")), "拒绝访问");
  // 非 Error 的值同样处理：设置状态里的 message 是纯字符串。
  assert.match(clientErrorText("Devin settings already define non-local or invalid API endpoints; restore the previous values before applying this configuration"), /其它地址/);
  assert.equal(clientErrorText(null), "操作失败");
  // 任何已知原因都不该再带出服务端前缀。
  for (const raw of [
    "configuration error: Devin installation not found",
    "configuration error: enable the Devin gateway before configuring Devin",
    "configuration error: Codex has active requests; wait before changing its configuration",
  ]) {
    assert.doesNotMatch(clientErrorText(new Error(raw)), /error: /);
  }
});
