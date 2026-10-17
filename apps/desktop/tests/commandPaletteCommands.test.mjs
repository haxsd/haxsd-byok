// 命令面板的「打开调用详情」必须走 store 的失败通道。
//
// 旧实现是 `void api.openCallDetails(...)`：服务不可达时 rejection 没人接（全仓也没有
// unhandledrejection 兜底），面板照常关闭、界面一点反馈都没有。同一动作在调用表与首页
// 走的是 `appStore.openCallDetails`，失败会变成提示条——命令面板不能是唯一的例外。

import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { installAppBackend, llmCallFixture } from "./appBackend.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

/** 等一个由 `run()` 触发的异步动作落定（run 本身是同步的）。 */
async function settled(predicate, timeoutMs = 1000) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    if (predicate()) return true;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  return predicate();
}

async function loadModules() {
  loader.invalidateAll();
  const { appStore } = await loader.load("/src/shared/store/appStore.ts");
  const { commandPaletteCommands } = await loader.load("/src/shell/commandPaletteCommands.ts");
  return { appStore, commandPaletteCommands };
}

function buildCommands(commandPaletteCommands, { calls, close }) {
  return commandPaletteCommands({
    calls,
    models: [],
    locale: "zh-CN",
    theme: "midnight",
    navigate: () => {},
    close,
  });
}

test("a failed open-call-details command surfaces the reason through the store", async () => {
  const { backend, state } = installAppBackend();
  state.calls = [llmCallFixture()];
  backend.route("POST", `${API_ROOT}/desktop/open-external-url`, () => ({ status: 500, body: { message: "the local service is not reachable" } }));
  const { appStore, commandPaletteCommands } = await loadModules();
  await appStore.refresh();

  let closed = 0;
  const command = buildCommands(commandPaletteCommands, { calls: state.calls, close: () => { closed += 1; } })
    .find((entry) => entry.id === `call-${llmCallFixture().call_id}`);
  assert.ok(command, "调用记录必须有一条命令");

  command.run();

  assert.equal(closed, 1, "命令执行后面板要关掉");
  assert.equal(
    await settled(() => appStore.getSnapshot().error !== null),
    true,
    "失败必须有人接：store 里应当留下错误，AppLayout 才能把它弹成提示条",
  );
  assert.equal(appStore.getSnapshot().error, "the local service is not reachable");
  assert.equal(backend.requestsFor("POST", `${API_ROOT}/desktop/open-external-url`).length, 1);
});

test("a successful command closes the panel without reporting an error", async () => {
  const { backend, state } = installAppBackend();
  state.calls = [llmCallFixture()];
  backend.route("POST", `${API_ROOT}/desktop/open-external-url`, () => ({ status: 204 }));
  const { appStore, commandPaletteCommands } = await loadModules();
  await appStore.refresh();

  let closed = 0;
  const command = buildCommands(commandPaletteCommands, { calls: state.calls, close: () => { closed += 1; } })
    .find((entry) => entry.id === `call-${llmCallFixture().call_id}`);

  command.run();
  await settled(() => backend.requestsFor("POST", `${API_ROOT}/desktop/open-external-url`).length === 1);

  assert.equal(closed, 1);
  assert.equal(appStore.getSnapshot().error, null, "成功不该留下错误");
});

test("the palette module never reaches for the bare api client", async () => {
  // 静态钉子：这条命令一旦退回裸 `api`，上一条用例就会红；这里再钉住模块的依赖面，
  // 免得有人以「顺手」为名把别的动作也接到裸 client 上。
  const source = await (await import("node:fs/promises")).readFile(
    new URL("../src/shell/commandPaletteCommands.ts", import.meta.url),
    "utf8",
  );
  assert.doesNotMatch(source, /^import \{[^}]*\bapi\b[^}]*\} from/m, "命令表不得引入裸 api 客户端");
  assert.match(source, /appStore\.openCallDetails/);
});
