import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

function model(overrides = {}) {
  return {
    model_hash: "f2581e6225494a89",
    sort_order: 1,
    display_name: "Devin E2E Stub",
    group_name: "E2E",
    type: "openai",
    base_url: "http://127.0.0.1:44120",
    use_full_url: false,
    api_key_configured: true,
    tooltip_data: "Local SSE",
    model_id: "stub-model",
    reasoning_effort: null,
    openai_endpoint: "/v1/chat/completions",
    openai_extra_params_enabled: false,
    openai_extra_params: {},
    custom_headers_enabled: false,
    custom_headers: {},
    anthropic_extra_params_enabled: false,
    anthropic_extra_params: {},
    context_window_tokens: 128_000,
    max_completion_tokens: 4_096,
    anthropic_max_tokens: null,
    anthropic_thinking_effort: null,
    thinking_budget_tokens: null,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_000,
    ...overrides,
  };
}

function binding(overrides = {}) {
  return {
    model_uid: "swe-1-6-slow",
    model_hash: "f2581e6225494a89",
    display_name: "Devin E2E Stub",
    context_window_tokens: 128_000,
    enabled: true,
    kind: "standard",
    routes: [],
    active_route_id: null,
    ...overrides,
  };
}

test("configured model labels use remark and display name, never the internal hash", async () => {
  const { devinModelOptions } = await loader.load("/src/features/devin/devinModelOptions.ts");

  const options = devinModelOptions([
    model(),
    model({ model_hash: "bbbb2222", display_name: "备用模型", tooltip_data: "", group_name: null, base_url: "https://api.example.com" }),
    model({ model_hash: "cccc3333", display_name: "", tooltip_data: "只有备注" }),
    model({ model_hash: "dddd4444", display_name: "", tooltip_data: "", model_id: "raw-model-id" }),
  ]);

  assert.deepEqual(options.map((option) => option.label), [
    "Local SSE · Devin E2E Stub",
    "备用模型",
    "只有备注",
    "raw-model-id",
  ]);
  assert.equal(options[0].group, "E2E");
  assert.equal(options[1].group, "api.example.com");
  assert.ok(options.every((option) => !option.label.includes("f2581e62")));
});

test("an identical remark and display name are not repeated", async () => {
  const { devinModelOptionLabel } = await loader.load("/src/features/devin/devinModelOptions.ts");

  assert.equal(devinModelOptionLabel(model({ display_name: "本地模型", tooltip_data: "本地模型" })), "本地模型");
  assert.equal(devinModelOptionLabel(model({ display_name: "DeepSeek flash", tooltip_data: "DeepSeek" })), "DeepSeek flash");
});

test("default model options show both the Devin name and the answering model", async () => {
  const { devinDefaultModelOptions } = await loader.load("/src/features/devin/devinModelOptions.ts");

  const options = devinDefaultModelOptions(
    [
      binding(),
      binding({ model_uid: "disabled-uid", enabled: false }),
      binding({ model_uid: "compact-uid", kind: "context_compression" }),
      binding({ model_uid: "missing-uid", model_hash: "gone" }),
    ],
    [model()],
    [
      { uid: "swe-1-6-slow", name: "SWE-1.6 Slow" },
      { uid: "missing-uid", name: "SWE-1.7" },
    ],
  );

  assert.deepEqual(options.map((option) => option.value), ["swe-1-6-slow", "missing-uid"]);
  assert.equal(options[0].label, "SWE-1.6 Slow → Local SSE · Devin E2E Stub");
  assert.equal(options[0].group, "E2E");
  assert.equal(options[1].label, "SWE-1.7 → 模型已删除或不可用");
  assert.equal(options[1].group, "模型库");
});
