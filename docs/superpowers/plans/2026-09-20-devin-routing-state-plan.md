# Devin Routing State Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add backward-compatible Devin model route state, manual candidate-route switching, and explicit context-compression bindings to the independent `haxsd byok` product.

**Architecture:** Keep the existing Devin UID binding as the control-plane identity and retain its legacy primary `model_hash`. Add optional binding kind, route candidates, and active-route selection with serde defaults; the gateway resolves the effective model hash while the catalog continues to expose one stable Devin UID per binding. The desktop Devin page edits this persisted state, but automatic provider retries and official Devin routing remain out of scope until black-box protocol evidence exists.

**Tech Stack:** Rust 2021, Axum, serde/serde_json, SQLx JSON service settings, React/TypeScript, SCSS modules, existing Cargo and npm verification scripts.

**Spec:** `docs/superpowers/specs/2026-09-20-devin-routing-state-design.md`

## Global Constraints

- Work only on `feat/devin-router`; never modify or merge into `main`.
- Cursor behavior stays at the existing 1.0.0 baseline; do not port newer Cursor commits.
- Preserve old Devin settings JSON: absent new fields deserialize to standard binding, no routes, and legacy `model_hash` as the effective route.
- Reject empty/duplicate route IDs, empty route model hashes, and active routes that are missing or disabled.
- Do not implement official Devin direct routing, automatic provider retry, upstream catalog fetching, or invented Devin wire fields in this phase.
- `AssignModel` tokens remain scoped to Devin model UIDs, not internal route IDs.
- Keep the existing loopback-only gateway, authentication, Connect wire conversion, tool forwarding, streaming response path, and receipt-backed host patch behavior.
- Do not commit or push the pre-existing uncommitted `haxsd byok` packaging changes with an implementation task.

---

### Task 1: Add backward-compatible Devin route state

**Files:**
- Modify: `server/src/devin/mod.rs`
- Modify: `server/src/devin/request.rs` (test fixture completion required by the new fields)
- Modify: `server/src/devin/catalog.rs` (test fixture completion required by the new fields)
- Test: inline unit tests in `server/src/devin/mod.rs`

**Interfaces:**
- Produces `DevinBindingKind`, `DevinRoute`, `DevinModelBinding::effective_model_hash`, and route validation used by the gateway and settings store.
- Consumes the existing `DevinSettings`, `DevinModelBinding`, serde derives, and `Result`/`Error` conventions.

- [ ] **Step 1: Write failing tests for old JSON compatibility and effective route selection**

Add tests that deserialize an old binding JSON with only `model_uid`, `model_hash`, `display_name`, `context_window_tokens`, and `enabled`; assert standard kind, empty routes, no active route, and the legacy hash as the effective hash. Add a second test with two routes that asserts the enabled active route hash is selected.

- [ ] **Step 2: Run the focused tests and verify the expected failure**

Run:

```text
cargo test -p cursor-server --lib devin::tests -- --nocapture
```

Expected: compilation/test failure because the new types, fields, and effective-route method do not exist yet.

- [ ] **Step 3: Write failing validation tests**

Add tests for duplicate route IDs, empty route IDs, empty route hashes, and an active route that is missing or disabled. Keep the existing duplicate Devin UID and empty primary hash checks intact.

- [ ] **Step 4: Implement the minimal route state model**

Add serde-defaulted enums/structs with snake-case JSON names. Keep existing fields unchanged, initialize new fields in `DevinModelBinding::new`, implement `effective_model_hash`, and extend `DevinSettings::validate` with the exact route invariants. A binding with no routes must continue to use its legacy `model_hash`.

- [ ] **Step 5: Run the focused tests and verify green**

Run:

```text
cargo test -p cursor-server --lib devin::tests -- --nocapture
```

Expected: all Devin settings tests pass, including the legacy JSON and invalid-route cases.

- [ ] **Step 6: Commit only Task 1 files**

```text
git add server/src/devin/mod.rs server/src/devin/request.rs server/src/devin/catalog.rs
git commit -m "feat: add Devin route state"
```

### Task 2: Resolve the selected route in the Devin gateway

**Files:**
- Modify: `server/src/devin/gateway.rs`
- Modify: `server/src/devin/request.rs` (use the selected route for invocation validation)
- Test: inline gateway/catalog tests in `server/src/devin/gateway.rs` and `server/src/devin/catalog.rs` as needed

**Interfaces:**
- Consumes `DevinModelBinding::effective_model_hash` from Task 1.
- Produces gateway behavior where `GetChatMessage` loads the selected route's model record while `AssignModel` still returns a token for the Devin UID.

- [ ] **Step 1: Add a failing route-resolution regression test**

Create a focused test for the gateway's binding-to-model-hash helper or the smallest pure helper extracted from `handle_request`: a binding with an active route must resolve the active route hash, while an old binding resolves its legacy hash. Assert that the Devin UID remains unchanged.

- [ ] **Step 2: Run the focused gateway tests and verify red**

Run:

```text
cargo test -p cursor-server --lib devin:: -- --nocapture
```

Expected: failure because the gateway still reads `binding.model_hash` directly.

- [ ] **Step 3: Implement effective-hash resolution**

Replace the direct model-hash lookup in `handle_request` with the Task 1 effective-route method. Keep all authentication, assignment-token, request parsing, provider streaming, and Connect response paths unchanged. Do not add retry or official-route behavior.

- [ ] **Step 4: Add catalog persistence coverage**

Add a catalog test showing that rewriting/generating a catalog keeps the configured Devin UID and does not mutate the binding's active route, candidate order, disabled state, or compression kind. The test must inspect the original settings after catalog generation rather than only the encoded bytes.

- [ ] **Step 5: Run the focused gateway/catalog tests and verify green**

Run:

```text
cargo test -p cursor-server --lib devin:: -- --nocapture
```

- [ ] **Step 6: Commit only Task 2 files**

```text
git add server/src/devin/gateway.rs server/src/devin/catalog.rs
git commit -m "feat: use selected Devin route in gateway"
```

### Task 3: Add Devin route editing to the desktop settings page

**Files:**
- Modify: `apps/desktop/src/shared/api.ts`
- Modify: `apps/desktop/src/features/devin/DevinSettingsPage.tsx`
- Modify: `apps/desktop/src/features/devin/DevinSettingsPage.module.scss`
- Modify: `apps/desktop/src/i18n/locales/en-US.json`
- Modify: `apps/desktop/src/i18n/locales/zh-CN.json`
- Modify: `apps/desktop/src/i18n/generated/catalog.json`

**Interfaces:**
- Consumes the JSON field names from Task 1: `kind`, `routes`, and `active_route_id`.
- Produces a Devin-only editor that preserves legacy bindings, lets users add/remove routes, selects the active route, and labels compression bindings.

- [ ] **Step 1: Update TypeScript types and add a failing type-level usage**

Add `DevinBindingKind`, `DevinRoute`, and the new `DevinModelBinding` fields. Update the empty settings object and binding creation path to construct a valid route-aware value. Use the new fields in the page before adding the implementation helpers so `npm run typecheck` fails for the missing editor logic.

- [ ] **Step 2: Run the desktop typecheck and verify red**

Run:

```text
npm run typecheck
```

Expected: failure from incomplete route-aware page state or missing component references.

- [ ] **Step 3: Implement legacy-safe route helpers**

Add page-local helpers that display an old binding's legacy `model_hash` as a synthetic primary route without changing it until the user edits the binding. Adding a candidate must materialize the synthetic route, set the selected route explicitly, and never silently switch the active route.

- [ ] **Step 4: Implement the route editor UI**

Add binding-kind selection, route model/label/enabled controls, active-route selection, and add/remove actions using existing `Select`, `TextInput`, `Switch`, `Button`, and `FormField` components. Keep host patch controls and current Devin enable/port/auth behavior unchanged. Add responsive SCSS only for the new route rows.

- [ ] **Step 5: Update translations and run the frontend checks**

Add the new route/kind labels to both locale files, regenerate the catalog using the existing i18n script, then run:

```text
npm run check
```

Expected: typecheck, node typecheck, and Vite build pass.

- [ ] **Step 6: Commit only Task 3 files**

```text
git add apps/desktop/src/shared/api.ts apps/desktop/src/features/devin/DevinSettingsPage.tsx apps/desktop/src/features/devin/DevinSettingsPage.module.scss apps/desktop/src/i18n/locales/en-US.json apps/desktop/src/i18n/locales/zh-CN.json apps/desktop/src/i18n/generated/catalog.json
git commit -m "feat: add Devin route editor"
```

### Task 4: Document the state model and run the complete regression gate

**Files:**
- Modify: `docs/devin-integration.md`
- Test: `server/src/devin/mod.rs`, `server/src/devin/gateway.rs`, `server/src/devin/catalog.rs`, and existing workspace tests

**Interfaces:**
- Consumes the completed route model and UI behavior from Tasks 1–3.
- Produces user-facing Devin documentation and fresh verification evidence for the isolated branch.

- [ ] **Step 1: Add a documentation regression checklist**

Update the Devin integration guide with legacy settings compatibility, standard versus context-compression bindings, manual route switching, and the explicit absence of official direct routing and automatic retries.

- [ ] **Step 2: Run formatting and focused Rust tests**

Run:

```text
cargo fmt --all -- --check
cargo test -p cursor-server devin -- --nocapture
```

Expected: both commands exit successfully with zero failed tests.

- [ ] **Step 3: Run workspace and frontend verification**

Run:

```text
cargo check --workspace --all-targets
npm run check
git diff --check
```

Expected: all commands exit successfully; any existing non-fatal build warnings are recorded but no errors remain.

- [ ] **Step 4: Verify branch isolation and changed-file scope**

Confirm the current branch is `feat/devin-router`, `origin/main` is unchanged, and no Cursor source path was modified by the Devin routing commits. Do not push or create a pull request in this plan.

- [ ] **Step 5: Commit documentation only**

```text
git add docs/devin-integration.md
git commit -m "docs: describe Devin route state"
```
