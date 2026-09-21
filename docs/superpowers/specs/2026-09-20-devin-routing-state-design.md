# Devin Routing State Design

**Date:** 2026-09-20  
**Status:** Approved direction; implementation pending

## Goal

Improve the Devin integration on `feat/devin-router` by learning the safe,
state-management parts of the upstream Devin Model Router releases while
keeping Cursor behavior frozen at the existing 1.0.0 baseline.

## Scope and isolation

This design applies only to the independent `haxsd byok` product on
`feat/devin-router`.

- Do not modify Cursor routing, Cursor protocol handling, or Cursor UI behavior.
- Do not merge the Devin branch into `main`.
- Do not cherry-pick the newer Cursor commits from `origin/main`.
- Do not reuse the Cursor release manifest, updater channel, or product storage.
- Keep the current Devin Connect wire adapter, tool forwarding, streaming
  response conversion, and receipt-backed host patch boundary intact.

## Upstream lessons and deliberate limits

The current upstream release notes emphasize three safe lessons: preserve
existing model mappings when refreshing a catalog, keep context-compression
configuration independent from ordinary routes, and represent multiple
candidate routes without silently replacing the active route.

The upstream official/direct route is deliberately out of scope for this
phase. `haxsd byok` currently owns a local relay backed by the existing model
store; it does not have enough verified Devin wire observations to reproduce
official account routing safely. Automatic fallback and upstream catalog
fetching therefore remain future work after real Devin black-box captures.

## Current architecture

`DevinSettings` is stored as one JSON value in the existing service-settings
table. Each `DevinModelBinding` maps a Devin model UID to one Cursor BYOK model
hash. The gateway resolves that binding, creates a provider invocation, and
converts provider events back to Devin Connect frames. The catalog is generated
from persisted bindings, so the persisted settings are already the source of
truth and must never be replaced by a transient catalog response.

## Design

### 1. Backward-compatible binding state

Keep the existing `model_uid` and `model_hash` fields as the legacy primary
route. Add serde-defaulted fields:

```rust
pub enum DevinBindingKind {
    Standard,
    ContextCompression,
}

pub struct DevinRoute {
    pub route_id: String,
    pub model_hash: String,
    pub label: String,
    pub enabled: bool,
}

pub struct DevinModelBinding {
    // Existing fields remain stable.
    pub model_uid: String,
    pub model_hash: String,
    pub display_name: String,
    pub context_window_tokens: Option<u64>,
    pub enabled: bool,

    // New fields use serde defaults for old settings rows.
    pub kind: DevinBindingKind,
    pub routes: Vec<DevinRoute>,
    pub active_route_id: Option<String>,
}
```

For old settings, an empty `routes` list and `None` active route resolve to the
existing `model_hash`. For new settings, `active_route_id` selects one enabled
route and its `model_hash` becomes the provider model. The legacy primary hash
is retained as a fallback representation so old databases remain readable and
are not rewritten destructively.

Validation must reject empty route IDs, duplicate route IDs, empty model hashes,
and an active route that does not exist or is disabled. A compression binding
is still an ordinary Devin UID mapping at runtime; its `kind` is explicit
metadata so the UI and later wire work cannot accidentally overwrite it with a
normal route.

### 2. Stable persistence and manual route switching

The settings store continues to persist the full Devin settings value. Catalog
generation reads the persisted binding list and never mutates it. Saving a
model catalog or refreshing the settings page must preserve:

- the active route;
- route order and labels;
- disabled candidate routes;
- compression bindings;
- all known binding fields introduced by this phase.

The first implementation exposes manual route selection in the Devin settings
page. It does not automatically retry a failed provider stream with another
candidate. A streamed provider failure may already have emitted tokens or tool
events, so automatic retry requires a verified Devin request lifecycle and a
separate idempotency policy.

### 3. Gateway behavior

The gateway keeps the existing UID lookup and authentication flow. Once a
binding is resolved, it uses the binding's effective active route model hash.
`AssignModel` continues to issue a token for the Devin UID, not for an internal
route ID, so existing host integrations remain compatible.

The generated catalog continues to expose one Devin UID per binding. Route
labels and candidate hashes are local control-plane state and are not invented
as new Devin wire fields until a real Devin capture proves the field shape.

### 4. Desktop settings UI

The Devin settings page gains:

- a binding-kind selector (`Standard` or `Context compression`);
- a route list for each binding;
- a visible active-route selector;
- add/remove route actions using the existing model catalog;
- validation feedback before saving.

The existing one-model binding remains the default presentation for old
settings. Adding a route must not silently change the active route.

### 5. Documentation

`docs/devin-integration.md` will document the new state model, manual route
switching, compression binding semantics, and the intentional absence of
automatic official-route fallback in this phase.

## Testing strategy

Tests are written before production changes:

1. Rust settings tests prove old JSON deserializes, route validation rejects
   invalid state, and effective-route resolution selects the active route.
2. Catalog tests prove persisted bindings keep their UID and do not rewrite
   route state.
3. Gateway tests prove a request uses the selected route's model hash and that
   assignment tokens remain UID-scoped.
4. Frontend checks prove the Devin settings types and route editor compile.
5. Existing Cursor and server tests remain the regression gate; no Cursor
   source file is intentionally changed by this work.

## Acceptance criteria

- Old Devin settings continue to load without migration failure.
- A user can add a candidate route and manually select it.
- A context-compression binding is stored independently from standard routes.
- The selected route is used by the Devin gateway after restart.
- Catalog generation does not replace persisted mappings.
- Cursor behavior and the `main` branch are unchanged.
- Fresh Rust and frontend verification passes before any branch push.
