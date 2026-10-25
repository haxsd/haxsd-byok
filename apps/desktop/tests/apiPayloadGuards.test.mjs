// 服务端返回坏数据时的行为：列表里的坏行被丢掉，单个对象的坏结构变成一次可读的失败。
//
// 这些守卫存在的原因是一次真实的白屏：`/api/models` 里出现一个 null 时，命令面板在
// 渲染里抛错，React 卸掉整棵树——窗口只剩空白，没有任何恢复入口。所以这里断言的是
// "坏数据进不来"，而不是"界面能画出坏数据"。

import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, installBrowserDoubles, startModuleLoader } from "./harness.mjs";
import { installAppBackend, llmCallFixture } from "./appBackend.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

async function loadApi() {
  loader.invalidateAll();
  const module = await loader.load("/src/shared/api.ts");
  return module.api;
}

/** 收集 console.warn：守卫要留下可诊断的记录，但不该刷屏。 */
function captureWarnings(run) {
  const warnings = [];
  const original = console.warn;
  console.warn = (...args) => { warnings.push(args.join(" ")); };
  return Promise.resolve(run())
    .finally(() => { console.warn = original; })
    .then(() => warnings);
}

test("a null row in the model list is dropped instead of reaching the renderer", async () => {
  const { backend } = installAppBackend();
  const valid = { model_hash: "hash-1", model_id: "example-model", display_name: "Example" };
  backend.route("GET", `${API_ROOT}/models`, () => ({ body: [null, valid, "not an object"] }));

  const api = await loadApi();
  const warnings = await captureWarnings(async () => {
    assert.deepEqual(await api.models(), [valid]);
  });
  assert.equal(warnings.length, 1, "dropping rows must leave exactly one diagnosable warning");
  assert.match(warnings[0], /丢弃了 2 条/);
});

test("a list that is not an array reads as empty instead of throwing", async () => {
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/models`, () => ({ body: { models: [] } }));
  backend.route("GET", `${API_ROOT}/llm-calls`, () => ({ body: { calls: [] } }));

  const api = await loadApi();
  await captureWarnings(async () => {
    assert.deepEqual(await api.models(), []);
  });
  await captureWarnings(async () => {
    assert.deepEqual(await api.calls(), []);
  });
});

test("the call list keeps only rows the UI can identify", async () => {
  const { backend } = installAppBackend();
  const valid = llmCallFixture();
  backend.route("GET", `${API_ROOT}/llm-calls`, () => ({ body: [null, { status: "completed" }, valid] }));

  const api = await loadApi();
  await captureWarnings(async () => {
    assert.deepEqual(await api.calls(), [valid]);
  });
});

test("the plugin list keeps only rows with an id", async () => {
  const { backend } = installAppBackend();
  const plugin = { id: "example-plugin", name: "Example", providers: [], resources: [] };
  backend.route("GET", `${API_ROOT}/plugins`, () => ({ body: [null, plugin] }));

  const api = await loadApi();
  await captureWarnings(async () => {
    assert.deepEqual(await api.plugins(), [plugin]);
  });
});

test("a plugin without its provider and resource lists is dropped, not rendered", async () => {
  const { backend } = installAppBackend();
  const complete = { id: "ok-plugin", name: "OK", providers: [], resources: [] };
  // providers 缺失时 configuredPluginModels 会在 .flatMap 处抛错，整页跟着崩。
  backend.route("GET", `${API_ROOT}/plugins`, () => ({ body: [{ id: "broken-plugin", name: "Broken" }, complete] }));

  const api = await loadApi();
  await captureWarnings(async () => {
    assert.deepEqual(await api.plugins(), [complete]);
  });
});

test("a pricing payload whose models field is not an object reads as an empty price book", async () => {
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/settings/pricing`, () => ({ body: { models: null } }));

  const api = await loadApi();
  await captureWarnings(async () => {
    assert.deepEqual(await api.pricingSettings(), { models: {} });
  });
});

test("a broken overview becomes a readable failure, not a crash on token_usage_series", async () => {
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/overview`, () => ({ body: { metrics: { llm_calls: 3 } } }));

  const api = await loadApi();
  await captureWarnings(async () => {
    await assert.rejects(
      () => api.overview({ startMs: 0, endMs: 1 }),
      /读取失败/,
    );
  });
});

test("a call detail without a call object fails with a readable message", async () => {
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/llm-calls/call-1`, () => ({ body: {} }));

  const api = await loadApi();
  await captureWarnings(async () => {
    await assert.rejects(() => api.call("call-1"), /读取失败/);
  });
});

test("a call detail missing its response chunks reads as an empty response", async () => {
  const { backend } = installAppBackend();
  const call = llmCallFixture();
  backend.route("GET", `${API_ROOT}/llm-calls/call-1`, () => ({ body: { call, request: null } }));

  const api = await loadApi();
  const detail = await api.call("call-1");
  assert.deepEqual(detail.response_chunks, [], "the detail page renders chunks.map and must never get undefined");
  assert.equal(detail.call.call_id, "call-1");
});

test("a refresh that reads broken lists keeps every other section and reports the failure", async () => {
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/overview`, () => ({ body: null }));
  backend.route("GET", `${API_ROOT}/models`, () => ({ body: [{ model_hash: "hash-1", model_id: "m", display_name: "M" }] }));

  const api = await loadApi();
  await captureWarnings(async () => {
    await assert.rejects(() => api.overview(undefined), /读取失败/);
  });
  assert.equal((await api.models()).length, 1, "the other endpoints in the same batch still answer");
});

test("devin settings without a bindings list fail readably instead of breaking the page", async () => {
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/devin/settings`, () => ({ body: { enabled: true, bindings: null } }));

  const api = await loadApi();
  await captureWarnings(async () => {
    // 页面拿 bindings.map 画映射表：拿到 null 会在渲染里抛错，用户只看到"这个页面出错了"。
    await assert.rejects(() => api.devinSettings(), /读取失败/);
  });
});

test("devin settings drop bindings whose routes are not a list", async () => {
  const { backend } = installAppBackend();
  const binding = {
    model_uid: "byok-1",
    model_hash: "hash-1",
    display_name: "Example",
    context_window_tokens: null,
    enabled: true,
    kind: "standard",
    routes: [{ route_id: "route-1", model_hash: "hash-1", label: "Route", enabled: true }],
    active_route_id: "route-1",
  };
  backend.route("GET", `${API_ROOT}/devin/settings`, () => ({
    body: { enabled: true, auth_token: "", default_model_uid: "byok-1", bindings: [{ model_uid: "byok-2", routes: null }, binding] },
  }));

  const api = await loadApi();
  const warnings = await captureWarnings(async () => {
    assert.deepEqual((await api.devinSettings()).bindings, [binding]);
  });
  assert.equal(warnings.length, 1, "a dropped binding must leave exactly one diagnosable warning");
  assert.match(warnings[0], /丢弃了 1 条/);
});

  test("devin settings without the explicit default field fail readably", async () => {
    const { backend } = installAppBackend();
    backend.route("GET", `${API_ROOT}/devin/settings`, () => ({
      body: { enabled: true, auth_token: "", bindings: [] },
    }));

    const api = await loadApi();
    await captureWarnings(async () => {
      await assert.rejects(() => api.devinSettings(), /读取失败/);
    });
  });

test("codex settings keep only string model hashes", async () => {
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/codex/settings`, () => ({
    body: { enabled: true, port: 1, home: "C:/codex", model_hashes: ["hash-1", null, 7], default_model_hash: "hash-1" },
  }));

  const api = await loadApi();
  const warnings = await captureWarnings(async () => {
    assert.deepEqual((await api.codexSettings()).model_hashes, ["hash-1"]);
  });
  assert.match(warnings.join(" "), /2 个非字符串模型标识/);
});

test("a codex status without its listening flag reads as a failure, not as a false state", async () => {
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/codex/status`, () => ({ body: { enabled: true, port: 1234, configured: true } }));

  const api = await loadApi();
  await captureWarnings(async () => {
    // 缺 listening 会让界面把"没在监听"当事实显示，用户照着假状态去改配置。
    await assert.rejects(() => api.codexStatus(), /读取失败/);
  });
});

test("a devin status without its readiness flag reads as a failure, not as a false state", async () => {
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/devin/status`, () => ({
    body: {
      enabled: true,
      listening: true,
      bound_ports: null,
      calls: 0,
      last_call_success: null,
      last_call_at_ms: null,
    },
  }));

  const api = await loadApi();
  await captureWarnings(async () => {
    // ready 缺失时不能退回 listening：旧端口仍在监听会被误画成绿色。
    await assert.rejects(() => api.devinStatus(), /读取失败/);
  });
});

test("a codex status without the new conflicts list reads as no conflicts", async () => {
  const { backend } = installAppBackend();
  // 旧服务端没有 restorable / conflicts：缺 conflicts 时页面仍要能渲染（遍历它）。
  backend.route("GET", `${API_ROOT}/codex/status`, () => ({
    body: { enabled: true, listening: true, port: 43120, configured: true, config_conflict: false, home: "test-home", calls: 0, last_error: null },
  }));

  const api = await loadApi();
  const status = await api.codexStatus();
  assert.deepEqual(status.conflicts, [], "a missing conflicts field must not reach the renderer as undefined");
  assert.equal(status.restorable, true, "without the new field the old configured rule decides the restore button");
});

test("codex status drops conflict rows the card cannot render", async () => {
  const valid = { key: "web_search", applied: "\"disabled\"", current: "\"live\"" };
  const { backend } = installAppBackend();
  backend.route("GET", `${API_ROOT}/codex/status`, () => ({
    body: {
      enabled: true, listening: true, port: 43120, configured: true, config_conflict: false,
      restorable: true, conflicts: [null, { key: "web_search" }, valid], home: "test-home", calls: 0, last_error: null,
    },
  }));

  const api = await loadApi();
  const warnings = await captureWarnings(async () => {
    assert.deepEqual((await api.codexStatus()).conflicts, [valid]);
  });
  assert.equal(warnings.length, 1, "a dropped conflict must leave exactly one diagnosable warning");
  assert.match(warnings[0], /丢弃了 2 条/);
});

test("a saved devin settings response with a broken shape reports a save failure", async () => {
  const { backend } = installAppBackend();
  backend.route("PUT", `${API_ROOT}/devin/settings`, () => ({ body: { enabled: true, bindings: "not a list" } }));

  const api = await loadApi();
  await captureWarnings(async () => {
    await assert.rejects(
      () => api.setDevinSettings({ enabled: true, auth_token: "", api_port: 1, inference_port: 2, local_api_port: 3, upstream_api_url: "", bindings: [] }),
      /保存失败/,
    );
  });
});
