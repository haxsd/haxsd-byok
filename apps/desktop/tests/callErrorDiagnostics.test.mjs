// Behavior tests for the call error block and its sanitized diagnostics copy.
//
// The call list and the isolated detail window both render `CallFacts`. A failed
// call must keep its stable error code and full locator ID visible outside
// developer mode, and the copy action must never carry request bodies, response
// chunks, chat text, URL credentials, bearer tokens or API keys. Historical app
// versions were never stored per call, so the diagnostic must say `unknown`
// instead of reusing the currently running version.

import assert from "node:assert/strict";
import test from "node:test";
import { createElement as h } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createServer } from "vite";
import { desktopRoot, installBrowserDoubles } from "./harness.mjs";

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

const { setLocalePreference } = await load("/src/i18n/store.ts");
setLocalePreference("zh-CN");

const CALL_ID = "call-0123456789abcdef0123456789abcdef";

/** One call row with only the fields this feature reads and asserts. */
function call(overrides = {}) {
  return {
    call_kind: "provider_llm",
    client_source: "cursor",
    route: "local_byok",
    call_id: CALL_ID,
    run_id: "run-1",
    conversation_id: "conversation-1",
    provider_call_index: 1,
    model_hash: "hash-1",
    provider_type: "openai",
    provider_url: "https://api.example.com/v1",
    request_type: "responses",
    request_url: "https://api.example.com/v1/responses",
    model_id: "gpt-5",
    display_name: "主力模型",
    reasoning_effort: null,
    fast: null,
    status: "failed",
    finish_reason: null,
    created_at_ms: Date.UTC(2026, 10, 9, 7, 14, 32),
    ttfb_ms: null,
    ttfr_ms: null,
    ttft_ms: null,
    duration_ms: 1_200,
    input_tokens: null,
    output_tokens: null,
    total_tokens: null,
    cache_read_tokens: null,
    cache_write_tokens: null,
    reasoning_tokens: null,
    message_count: 12,
    tool_count: 3,
    http_status: 429,
    error_kind: "BYOK-UPSTREAM_HTTP",
    error_message: "upstream returned HTTP 429 (rate limited)",
    detailed: false,
    purpose: "chat",
    ...overrides,
  };
}

function visibleText(markup) {
  return markup.replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();
}

async function renderFacts(record) {
  const { CallFacts } = await load("/src/features/calls/CallFacts.tsx");
  return visibleText(renderToStaticMarkup(h(CallFacts, { call: record })));
}

test("diagnostic copy carries the requested fields and the true version split", async () => {
  const { buildErrorDiagnostic } = await load("/src/features/calls/errorDiagnostics.ts");
  const text = buildErrorDiagnostic(call(), "1.7.7");

  assert.match(text, /^client: cursor$/m);
  assert.match(text, /^time: 2026-11-09T07:14:32\.000Z$/m);
  assert.match(text, /^model: gpt-5$/m);
  assert.match(text, /^protocol: responses$/m);
  assert.match(text, /^code: BYOK-UPSTREAM_HTTP$/m);
  assert.match(text, new RegExp(`^call_id: ${CALL_ID}$`, "m"));
  assert.match(text, /^known_http_status: 429$/m);
  assert.match(text, /^reason: upstream returned HTTP 429 \(rate limited\)$/m);
  assert.match(text, /^occurred_app_version: unknown$/m, "no call-time version exists in the schema");
  assert.match(text, /^current_app_version: 1\.7\.7$/m);
});

test("unknown HTTP status and a legacy error kind stay honest", async () => {
  const { buildErrorDiagnostic } = await load("/src/features/calls/errorDiagnostics.ts");
  const text = buildErrorDiagnostic(call({ http_status: null, error_kind: "provider" }), null);

  assert.match(text, /^code: provider$/m, "legacy kinds are shown as-is, not guessed");
  assert.match(text, /^known_http_status: unknown$/m);
  assert.match(text, /^current_app_version: unknown$/m);
  assert.match(text, /^occurred_app_version: unknown$/m);
});

test("diagnostic copy redacts URL credentials, query tokens and auth material", async () => {
  const { buildErrorDiagnostic } = await load("/src/features/calls/errorDiagnostics.ts");
  const secretReason = [
    "POST https://alice:hunter2@api.example.com/v1?api_key=sk-live-1234567890&safe=ok",
    "Authorization: Basic dXNlcjpwYXNz",
    "Authorization: Bearer abc.def.ghi",
    "x-api-key: sk-ant-secret-123456",
    "api_key: plain-text-key",
    "https://example.com/callback#token=fragment-secret",
    '{"access_token":"ghp_1234567890abcdef","keep":"visible"}',
  ].join("\n");
  const text = buildErrorDiagnostic(call({ error_message: secretReason }), "1.7.7");

  for (const secret of [
    "hunter2",
    "sk-live-1234567890",
    "dXNlcjpwYXNz",
    "abc.def.ghi",
    "sk-ant-secret-123456",
    "plain-text-key",
    "fragment-secret",
    "ghp_1234567890abcdef",
  ]) {
    assert.doesNotMatch(text, new RegExp(secret), `secret ${secret} must not reach the clipboard`);
  }
  assert.match(text, /\[redacted\]/);
  assert.match(text, /safe=ok/, "non-secret query parameters stay readable");
  assert.match(text, /"keep":"visible"/);
});

test("diagnostic copy is a whitelist, not a dump of the call detail payload", async () => {
  const { buildErrorDiagnostic } = await load("/src/features/calls/errorDiagnostics.ts");
  const leaked = call({
    error_message: "boom",
    request_body: "REQUEST-BODY-SECRET",
    response_chunks: [{ data: "RESPONSE-SECRET" }],
    messages: [{ content: "CHAT-SECRET" }],
  });
  const text = buildErrorDiagnostic(leaked, "1.7.7");

  assert.match(text, /^reason: boom$/m, "the diagnostic is never allowed to be empty");
  assert.doesNotMatch(text, /REQUEST-BODY-SECRET/);
  assert.doesNotMatch(text, /RESPONSE-SECRET/);
  assert.doesNotMatch(text, /CHAT-SECRET/);
});

test("successful copy sends the generated diagnostic and reports ok", async () => {
  const { buildErrorDiagnostic, copyErrorDiagnostic } = await load("/src/features/calls/errorDiagnostics.ts");
  let copied = "";
  const result = await copyErrorDiagnostic(call(), "1.7.7", async (text) => { copied = text; });
  assert.deepEqual(result, { ok: true });
  assert.equal(copied, buildErrorDiagnostic(call(), "1.7.7"));
  assert.match(copied, /^current_app_version: 1\.7\.7$/m);
});

test("copy failure keeps the clipboard reason for the UI", async () => {
  const { copyErrorDiagnostic } = await load("/src/features/calls/errorDiagnostics.ts");
  const result = await copyErrorDiagnostic(call(), "1.7.7", async () => {
    throw new Error("clipboard permission denied");
  });
  assert.deepEqual(result, { ok: false, reason: "clipboard permission denied" });
});

test("non-developer error block shows the stable code and the full call ID", async () => {
  const text = await renderFacts(call());
  assert.match(text, /错误码 BYOK-UPSTREAM_HTTP/);
  assert.match(text, new RegExp(`定位编号 ${CALL_ID}`), "the locator ID must not be truncated");
  assert.match(text, /复制错误诊断/);
  assert.match(text, /upstream returned HTTP 429/);
});

test("legacy records show their existing kind and never invent a BYOK code", async () => {
  const text = await renderFacts(call({ error_kind: "internal", error_message: "legacy failure" }));
  assert.match(text, /错误码 internal/);
  assert.doesNotMatch(text, /BYOK-/, "an old internal failure must not be reclassified");
  assert.match(text, new RegExp(`定位编号 ${CALL_ID}`));
});

test("a failed legacy record without error fields still exposes its locator ID", async () => {
  const text = await renderFacts(call({ error_kind: null, error_message: null }));
  assert.match(text, /错误码 未知/);
  assert.match(text, new RegExp(`定位编号 ${CALL_ID}`));
  assert.match(text, /复制错误诊断/);
});
