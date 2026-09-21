# Devin adapter design

## Goal

Add an optional Devin-compatible local model gateway to cursor-byok. Devin requests must be translated into the existing provider-independent model types and routed through the configured cursor-byok model table, while the existing Cursor gateway, CursorHarness, RunEngine, and tool executor remain behaviorally unchanged when Devin support is disabled.

## Scope

### Included

- A persisted `DevinSettings` object with an explicit `enabled` flag, three loopback ports, and model UID → cursor-byok model hash bindings.
- A separate Devin control API under `/__byok-api__/api/devin/*`.
- Connect framing and the subset of Devin protobuf wire fields required by `GetChatMessage`, streaming text/thinking/tool deltas, usage, stop, and error frames.
- Translation between Devin messages/tools and `ModelInvocation`/`ModelEvent`.
- Separate loopback listeners for the Devin API and inference surfaces; the listeners do not start when `enabled=false`.
- Fixture/unit/integration tests that prove Cursor routes and listeners are unchanged when the flag is off.
- A later opt-in host integration boundary for catalog rewriting and endpoint patch/restore, guarded by version anchors and backups.

### Excluded from the first gateway milestone

- Devin Model Router commercial license code, public keys, lease logic, or server calls.
- Vendor SQLite mutation, SSH remote patching, status-extension installation, and Responses WebSocket continuation.
- Any automatic modification of Devin/Windsurf files.
- Reusing Cursor's CA/MITM harness for Devin.
- Running Devin's executable or sending credentials to official Devin/Windsurf/Codeium hosts.

## Architecture

```text
Devin host
  ├─ API surface ────────┐
  └─ inference surface ──┴─> Devin gateway (loopback, token, 24 MiB limit)
                              ├─ wire decode / Connect unwrap
                              ├─ Devin UID binding lookup
                              ├─ ModelInvocation mapper
                              └─ shared ProviderRouter
                                   ├─ existing model store + credentials
                                   └─ existing OpenAI/Anthropic/plugin adapters
                              └─ ModelEvent → Devin Connect frames

Cursor host ───────────────> existing Cursor router / CursorHarness / RunEngine
                              (no shared route mutation)
```

The Devin gateway owns only protocol translation and lifecycle. Devin remains the tool executor: tool definitions, tool calls, and tool results are represented in the request/response stream but never executed by cursor-byok's Cursor tool runtime.

## Configuration contract

```rust
pub struct DevinSettings {
    pub enabled: bool,
    pub api_port: u16,
    pub inference_port: u16,
    pub local_api_port: u16,
    pub bindings: Vec<DevinModelBinding>,
}

pub struct DevinModelBinding {
    pub model_uid: String,
    pub model_hash: String,
    pub display_name: String,
    pub context_window_tokens: Option<u64>,
    pub enabled: bool,
}
```

Defaults are disabled with ports `43110`, `43111`, and `43112`. API keys are never duplicated into this object; `model_hash` resolves through `Store::model` and the existing credential storage.

## Protocol contract

The adapter accepts Connect envelopes with optional gzip, validates a maximum unwrapped body of 24 MiB, and parses protobuf wire types 0, 1, 2, and 5. It recognizes the Devin message fields already established by the reference app:

- request system field 2, message field 3, tool field 10, tool choice field 12;
- cascade/prompt/model/execution fields 16/17/21/22;
- message source/text/token/tool calls/tool result/cache/images/thinking/signature fields 1–13 and metadata fields 15/16/18/19;
- response text/thinking/signature/tool/stop/usage/response-id/model fields used by the reference app.

Unknown fields are preserved only where a response envelope must be forwarded unchanged; model request parsing does not guess unknown semantic fields. Ambiguous or missing model bindings fail closed with a Connect error frame.

## Lifecycle and security

- Devin listeners bind only to `127.0.0.1`.
- The control API uses the existing local control authorization boundary; gateway route control uses a per-process token.
- No new listener is created while `enabled=false`.
- Start/stop is owned by `App`; shutdown cancels both Devin listeners before Cursor shutdown completes.
- No upstream forwarding exists in the first milestone, preventing accidental credential or request leakage to vendor hosts.
- Host patching, when implemented, will be a separate module with exact version anchors, SHA-256 backup validation, atomic replacement, and restore-only-on-matching-backup behavior.

## Error and accounting rules

- Invalid wire, body-too-large, missing binding, disabled binding, unknown model, provider 401/403/429/5xx, and cancellation map to stable Connect error codes.
- `ModelEvent::ProviderReplayState` is preserved for subsequent requests; Anthropic thinking blocks/signatures are emitted in response metadata where available.
- Provider usage is mapped to Devin input/output/cache fields without adding cache counts twice. If the provider omits input usage, the request's declared context token count is marked as estimated.
- Every Devin invocation uses `run_id=devin:<executionId>` and `conversation_id=devin:<cascadeId>` and is recorded through the existing provider recorder.

## Verification requirements

1. Pure wire and mapping tests pass with no network access.
2. Gateway tests prove disabled settings produce no listener and enabled settings bind only loopback ports.
3. A fake provider stream proves text, thinking, tool-call, usage, stop, and error framing.
4. Existing `cargo test --workspace` and desktop checks pass on the new branch.
5. The live `codex/storage-retention` checkout remains byte-for-byte unchanged by implementation work.
