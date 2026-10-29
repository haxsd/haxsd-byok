import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const bulk = () => loader.load("/src/features/models/modelBulkAdd.ts");

function model(overrides = {}) {
  return {
    model_hash: "hash-1",
    sort_order: 1,
    display_name: "Existing",
    group_name: null,
    type: "openai",
    base_url: "https://api.example.com/",
    use_full_url: false,
    api_key_configured: true,
    tooltip_data: "Existing",
    model_id: "existing",
    reasoning_effort: null,
    openai_endpoint: "/v1/chat/completions",
    openai_extra_params_enabled: false,
    openai_extra_params: {},
    custom_headers_enabled: false,
    custom_headers: {},
    anthropic_extra_params_enabled: false,
    anthropic_extra_params: {},
    context_window_tokens: null,
    max_completion_tokens: null,
    anthropic_max_tokens: null,
    anthropic_thinking_effort: null,
    thinking_budget_tokens: null,
    created_at_ms: 0,
    updated_at_ms: 0,
    ...overrides,
  };
}

function input(overrides = {}) {
  return {
    sort_order: 1,
    display_name: "",
    group_name: null,
    type: "openai",
    base_url: "https://api.example.com",
    use_full_url: false,
    api_key: "sk-test",
    tooltip_data: "",
    model_id: "",
    reasoning_effort: null,
    openai_endpoint: "/v1/chat/completions",
    openai_extra_params_enabled: false,
    openai_extra_params: {},
    custom_headers_enabled: false,
    custom_headers: {},
    anthropic_extra_params_enabled: false,
    anthropic_extra_params: {},
    context_window_tokens: null,
    max_completion_tokens: null,
    anthropic_max_tokens: null,
    anthropic_thinking_effort: null,
    thinking_budget_tokens: null,
    ...overrides,
  };
}

test("discovery candidates are trimmed, unique, and keep provider order", async () => {
  const { discoveryCandidates } = await bulk();
  assert.deepEqual(discoveryCandidates([" z ", "", "a", "z", " a "]), ["z", "a"]);
});

test("existing models are matched by protocol, normalized endpoint, and exact model id", async () => {
  const { existingDiscoveryIds } = await bulk();
  const library = [
    model(),
    model({ model_hash: "hash-2", model_id: "responses", openai_endpoint: "/v1/responses" }),
    model({ model_hash: "hash-3", model_id: "anthropic", type: "anthropic", openai_endpoint: "" }),
  ];
  assert.deepEqual([...existingDiscoveryIds(library, input())], ["existing"]);
});

test("bulk inputs share provider settings and assign deterministic identity and order", async () => {
  const { bulkModelInputs } = await bulk();
  const base = input();
  const rows = bulkModelInputs(base, ["a", "b"], 7, "Example");
  assert.deepEqual(rows.map((row) => row.model_id), ["a", "b"]);
  assert.deepEqual(rows.map((row) => row.sort_order), [7, 8]);
  assert.deepEqual(rows.map((row) => row.display_name), ["a", "b"]);
  assert.deepEqual(rows.map((row) => row.tooltip_data), ["Example · a", "Example · b"]);
  assert.equal(rows[0].api_key, "sk-test");
  assert.equal(rows[0].base_url, base.base_url);
});

test("the discovery picker never offers a select-all shortcut", () => {
  const source = fs.readFileSync(
    new URL("../src/features/models/ModelDiscoveryPicker.tsx", import.meta.url),
    "utf8",
  );
  assert.doesNotMatch(source, /全选|Select all/i);
});
