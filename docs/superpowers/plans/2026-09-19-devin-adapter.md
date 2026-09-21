# Devin adapter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in Devin-compatible local gateway that routes Devin model requests through cursor-byok's existing model store and ProviderRouter without changing Cursor behavior.

**Architecture:** Add a `server::devin` protocol/lifecycle module and a separate Devin API surface. The adapter translates Devin wire messages into `ModelInvocation`, calls the shared `ProviderRouter`, and frames `ModelEvent` output back to Devin. Configuration and credentials remain in the existing store; Cursor routes and CursorHarness remain separate.

**Tech Stack:** Rust 2021, Axum 0.8, Tokio, SQLx SQLite, Prost-compatible protobuf wire rules, existing provider/model abstractions, serde JSON.

**Spec:** `docs/superpowers/specs/2026-09-19-devin-adapter-design.md`

## Global Constraints

- Devin support is disabled by default and creates no listener while disabled.
- Devin listeners bind only to `127.0.0.1` and use explicit 24 MiB body limits.
- API keys are read only through existing `Store::model`; Devin settings contain model hashes, never credentials.
- Cursor API routes, CursorHarness, RunEngine, provider implementations, and tool execution behavior remain unchanged.
- Do not copy commercial Devin license, public-key, lease, vendor SQLite, SSH, or status-extension code.
- Every new production function has a test written and observed failing before implementation.
- All commands run from the isolated checkout `D:\cursor-byok\byok-dev\cursor-byok-devin-router`.

---

### Task 1: Devin settings and binding persistence

**Files:**
- Modify: `server/src/store/settings.rs`
- Modify: `server/src/control/mod.rs`
- Create: `server/src/control/devin.rs`
- Test: `server/src/store/settings.rs` and `server/src/control/devin.rs` module tests

**Interfaces:**
- `Store::devin_settings() -> Result<DevinSettings>`
- `Store::set_devin_settings(DevinSettings) -> Result<DevinSettings>`
- `DevinSettings::validate() -> Result<()>`
- Control routes `GET/PUT /__byok-api__/api/devin/settings`

- [ ] **Step 1: Write failing settings tests**

Test that the default is disabled with ports `43110/43111/43112`, that a binding with empty UID/hash is rejected, that duplicate UIDs are rejected, and that a valid binding round-trips through the store JSON setting.

- [ ] **Step 2: Run the focused tests and confirm the expected missing-type or missing-method failures**

Run: `cargo test -p cursor-server devin_settings -- --nocapture`

- [ ] **Step 3: Implement the serde settings types and Store read/write methods**

Use the existing `service_settings(setting_key, value_json, updated_at_ms)` pattern. Keep API keys out of the new JSON and validate all ports as non-zero loopback listener ports.

- [ ] **Step 4: Add control handlers and routes**

Return the public settings shape without secrets. Reuse the existing control router authorization and return the same JSON error envelope used by other control endpoints.

- [ ] **Step 5: Re-run focused tests and inspect the diff**

Run: `cargo test -p cursor-server devin_settings -- --nocapture`

- [ ] **Step 6: Commit**

Run: `git add server/src/store/settings.rs server/src/control/mod.rs server/src/control/devin.rs && git commit -m "feat: persist Devin router settings"`

### Task 2: Connect framing and Devin wire codec

**Files:**
- Create: `server/src/devin/mod.rs`
- Create: `server/src/devin/wire.rs`
- Test: `server/src/devin/wire.rs` module tests

**Interfaces:**
- `wire::unwrap_request(body: &[u8], content_encoding: Option<&str>) -> Result<Vec<u8>>`
- `wire::frame(payload: &[u8], compress: bool) -> Result<Vec<u8>>`
- `wire::end_frame(error: Option<ConnectError>) -> Vec<u8>`
- `wire::parse_fields(payload: &[u8]) -> Result<Vec<Field>>`
- `wire::serialize_fields(fields: &[Field]) -> Result<Vec<u8>>`

- [ ] **Step 1: Write failing codec tests**

Cover a raw protobuf varint, nested length-delimited message, fixed32/fixed64, gzip Connect frame, invalid varint, truncated payload, 24 MiB boundary, and error end frame JSON.

- [ ] **Step 2: Run `cargo test -p cursor-server devin::wire` and verify the failures are due to missing codec behavior**

- [ ] **Step 3: Implement bounded varint parsing and field serialization**

Reject field zero, unsupported wire types, integer overflow, and lengths past the configured body limit. Use `flate2` already present in dev dependencies only if it is promoted to the normal dependency list; otherwise use the existing compression dependency that is already available to production code.

- [ ] **Step 4: Implement Connect envelope unwrap/frame/end behavior**

Honor content encoding and the five-byte envelope flags used by the reference app. Never silently accept a declared length beyond the input buffer.

- [ ] **Step 5: Re-run codec tests and commit**

Run: `cargo test -p cursor-server devin::wire -- --nocapture`

Commit: `git add server/src/devin server/Cargo.toml server/Cargo.lock && git commit -m "feat: add Devin Connect wire codec"`

### Task 3: Request and response mapping

**Files:**
- Create: `server/src/devin/request.rs`
- Create: `server/src/devin/response.rs`
- Test: module tests in both files and fixture bytes under `server/tests/fixtures/devin/`

**Interfaces:**
- `request::parse_chat_request(payload: &[u8]) -> Result<DevinChatRequest>`
- `request::to_invocation(request: DevinChatRequest, binding: &DevinModelBinding, model: &ModelConfig) -> Result<ModelInvocation>`
- `response::stream_event(event: ModelEvent, state: &mut ResponseState) -> Result<Vec<Vec<u8>>>`
- `response::finish(state: ResponseState, model_uid: &str) -> Result<Vec<u8>>`

- [ ] **Step 1: Write failing request tests**

Build field fixtures for system text, user text, assistant tool calls, tool results, images, thinking/signatures, tool definitions, model UID, cascade ID, execution ID, and declared token counts. Assert conversion to `PromptSpec`, `ProjectedMessage`, and `ModelSpec`.

- [ ] **Step 2: Run request tests and confirm they fail before implementation**

Run: `cargo test -p cursor-server devin::request -- --nocapture`

- [ ] **Step 3: Implement request parsing and model invocation mapping**

Use the existing `ContentPart`, `ProjectedContent`, `ToolCallContent`, `ToolResultContent`, and `ToolDefinition` types. Map Devin's tool executor ownership through unchanged; do not invoke Cursor tools.

- [ ] **Step 4: Write failing response tests**

Assert that text/thinking deltas frame correctly, tool arguments aggregate across deltas, usage maps input/output/cache fields once, and `Done` emits the correct stop reason. Add error mapping for missing model and provider errors.

- [ ] **Step 5: Implement event-to-frame state machine**

Aggregate tool calls by event index, extract available Anthropic signature blocks from replay state, and use stable response/message IDs for one invocation.

- [ ] **Step 6: Re-run mapping tests and commit**

Run: `cargo test -p cursor-server devin::request devin::response -- --nocapture`

Commit: `git add server/src/devin server/tests/fixtures/devin && git commit -m "feat: map Devin chat requests and provider events"`

### Task 4: Separate Devin gateway lifecycle

**Files:**
- Create: `server/src/devin/gateway.rs`
- Modify: `server/src/devin/mod.rs`
- Modify: `server/src/app.rs`
- Modify: `server/src/config.rs`
- Test: `server/src/devin/gateway.rs` module tests and app lifecycle tests

**Interfaces:**
- `DevinGateway::new(store: Store, provider: Arc<dyn Provider>, settings: DevinSettings) -> Result<Self>`
- `DevinGateway::serve(self, shutdown: CancellationToken) -> Result<()>`
- `DevinGateway::router(&self) -> axum::Router`

- [ ] **Step 1: Write failing lifecycle tests**

Prove disabled settings do not bind ports, enabled settings bind only loopback, a body over 24 MiB returns 413, and Cursor's existing service listener remains constructible with Devin disabled.

- [ ] **Step 2: Run lifecycle tests and verify the intended failures**

Run: `cargo test -p cursor-server devin::gateway -- --nocapture`

- [ ] **Step 3: Implement the separate gateway**

Use independent listeners for API and inference ports, a shared gateway state containing Store, Provider, settings, and a per-process control token. Do not merge Devin routes into `api::cursor::handlers::router` and do not add a generic fallback.

- [ ] **Step 4: Wire App startup/shutdown**

Clone the existing provider `Arc` before passing it to ControlService. Start Devin only when settings are enabled, cancel it during shutdown, and preserve existing Cursor harness shutdown ordering.

- [ ] **Step 5: Re-run focused and workspace tests**

Run: `cargo test -p cursor-server devin -- --nocapture` and `cargo test --workspace`

- [ ] **Step 6: Commit**

Run: `git add server/src/app.rs server/src/config.rs server/src/devin && git commit -m "feat: add isolated Devin gateway lifecycle"`

### Task 5: Devin model catalog and assignment surface

**Files:**
- Create: `server/src/devin/catalog.rs`
- Create: `server/src/devin/assignment.rs`
- Modify: `server/src/devin/gateway.rs`
- Test: catalog/assignment fixture tests

**Interfaces:**
- `catalog::rewrite_model_configs(payload: &[u8], settings: &DevinSettings, gateway_url: &str) -> Result<Vec<u8>>`
- `assignment::AssignmentSessions::issue(uid: &str) -> Assignment`
- `assignment::find_model_reference(payload: &[u8], headers: &HeaderMap, candidates: &HashSet<String>, sessions: &AssignmentSessions) -> ModelReference`

- [ ] **Step 1: Write failing catalog and assignment tests**

Cover model UID selection, 12-hour assignment expiry, nested protobuf UID discovery up to depth five and 2 MiB nested bytes, ambiguity rejection, gateway URL rewrite, and disabled bindings.

- [ ] **Step 2: Implement bounded catalog rewrite and assignment sessions**

Use the existing `wire` codec; never recursively scan beyond the documented bounds. Assignment tokens are opaque random values tied to a model UID and expiry; do not implement or copy the reference app's commercial JWT/license behavior.

- [ ] **Step 3: Add local API handlers and rerun tests**

Run: `cargo test -p cursor-server devin::catalog devin::assignment -- --nocapture`

- [ ] **Step 4: Commit**

Run: `git add server/src/devin && git commit -m "feat: add Devin model catalog bindings"`

### Task 6: Desktop Devin settings surface

**Files:**
- Modify: `apps/desktop/src/App.tsx`
- Modify: `apps/desktop/src/shell/AppLayout.tsx`
- Modify: `apps/desktop/src/shared/api.ts`
- Modify: `apps/desktop/src/demo/api.ts`
- Create: `apps/desktop/src/features/devin/DevinSettingsPage.tsx`
- Create: `apps/desktop/src/features/devin/DevinSettingsPage.module.scss`
- Test: `apps/desktop/src/features/devin/DevinSettingsPage.tsx` behavior through the repository's TypeScript/build checks

**Interfaces:**
- `GET/PUT /__byok-api__/api/devin/settings`
- UI fields for enable toggle, ports, binding rows, model selector, display name, context window, and per-binding enable toggle

- [ ] **Step 1: Add the typed API contract and compile-time test fixture**

Assert disabled-by-default rendering, no secret fields, validation errors for duplicate UIDs and invalid ports, and save/reload behavior through the demo API fixture in `apps/desktop/src/demo/api.ts`.

- [ ] **Step 2: Implement the isolated Devin settings page**

Keep Cursor settings components and model behavior unchanged. Use existing design tokens and accessible controls; do not use emoji as structural icons.

- [ ] **Step 3: Run desktop checks**

Run `npm run typecheck`, `npm run typecheck:node`, and `npm run build` from `apps/desktop` and record exact output.

- [ ] **Step 4: Commit**

Run: `git add apps/desktop/src && git commit -m "feat: add Devin router settings"`

### Task 7: Opt-in host patch/restore boundary

**Files:**
- Create: `server/src/devin/host_patch.rs`
- Create: `server/src/devin/host_status.rs`
- Modify: `server/src/control/devin.rs`
- Test: host patch fixture tests with synthetic vendor files only

**Interfaces:**
- `host_patch::inspect(path: &Path) -> Result<PatchStatus>`
- `host_patch::apply(path: &Path, ports: DevinPorts) -> Result<PatchReceipt>`
- `host_patch::restore(receipt: &PatchReceipt) -> Result<()>`
- `host_status::status(path: &Path) -> Result<HostStatus>`

- [ ] **Step 1: Write failing safe-patch tests**

Cover exact anchors, unsupported version, backup SHA-256 mismatch, atomic replacement, restore, and no write when any anchor is missing.

- [ ] **Step 2: Implement patching only against synthetic fixture files**

The module must never discover or modify a vendor path automatically. It receives an explicit path from the user, creates a verified backup, and fails closed on unknown content.

- [ ] **Step 3: Add explicit UI action and status reporting**

The action must be opt-in, show patch readiness, and keep Cursor harness status independent.

- [ ] **Step 4: Run patch fixture tests and commit**

Run: `cargo test -p cursor-server devin::host_patch -- --nocapture`

Commit: `git add server/src/devin server/src/control/devin.rs apps/desktop/src/features/devin && git commit -m "feat: add opt-in Devin host patch boundary"`

### Task 8: Final verification and branch review

**Files:**
- Modify: `docs/superpowers/plans/2026-09-19-devin-adapter.md` to check completed steps only after evidence
- Create: `docs/devin-integration.md`

- [ ] **Step 1: Verify live checkout isolation**

Run `git -C D:\cursor-byok\byok-dev\cursor-byok-upstream status --short --branch` and confirm the Cursor checkout is on a clean `main` with no Devin files.

- [ ] **Step 2: Run complete verification from the Devin worktree**

Run `cargo fmt --all -- --check`, `cargo test --workspace`, desktop typecheck/test commands, and the focused Devin fixture suite. Record exit codes and failure counts.

- [ ] **Step 3: Review branch diff and request code review**

Compare `ffec3f6..HEAD`, review all new Devin files, and dispatch a reviewer with the spec, plan, full diff, isolation constraint, and security constraints.

- [ ] **Step 4: Fix all Critical/Important findings and re-run the affected tests**

- [ ] **Step 5: Only then report the branch, files, test evidence, and remaining explicit opt-in limitations**
