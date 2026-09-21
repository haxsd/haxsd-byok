# Devin integration

This branch packages an independent product named `haxsd byok`. It adds an
optional Devin-compatible gateway to the existing Cursor model harness. The
Devin listener is isolated from the Cursor listener and is disabled by default.

This branch is intentionally isolated from `main`, which remains the Cursor
BYOK product line. Do not merge the branches or reuse the Cursor release/update
channel. The product uses its own Tauri identifier (`dev.haxsd.byok`) and
storage directory (`.haxsd-byok-devin-v3`).

## What is integrated

- Devin Connect request framing, gzip, protobuf-like fields, streaming text,
  thinking, tool calls, signatures, stop reasons, and usage.
- Devin model catalog responses backed by the existing Cursor BYOK model store.
- `AssignModel` sessions with opaque 12-hour tokens. The implementation does
  not copy the reference router's commercial license or JWT behavior.
- Devin model UID to Cursor BYOK model-hash bindings, persisted in the existing
  settings store. API keys remain in the existing model records.
- Optional candidate routes per binding and a binding kind that marks a Devin
  UID as a context-compression binding.
- The existing provider router and Devin tool definitions are reused. Devin
  remains responsible for executing its tools; Cursor BYOK supplies model
  inference and forwards model events.
- An opt-in host patch boundary for a known Devin/Windsurf `extension.js`.
  The user must provide an absolute file path. The patcher requires all four
  endpoint anchors, creates a SHA-256-verified backup, replaces atomically, and
  refuses unknown or partially patched files.

## Ports and setup

The settings page is available under the Devin section. Defaults are:

| Listener | Default |
| --- | ---: |
| API / catalog | `43110` |
| inference | `43111` |
| local API | `43112` |

Enable the gateway, add at least one enabled model binding, and save. The
process reads listener settings at startup, so restart haxsd byok after
changing enabled state or ports.

The gateway binds only to `127.0.0.1`. An optional token can be supplied with
`x-devin-router-token` or `Authorization: Bearer ...`. Requests larger than
24 MiB are rejected.

For native Devin integration, first inspect the exact host `extension.js` path
in the settings page. Only after the status reports a compatible clean version
and the Devin gateway is enabled should “应用补丁” be used. The receipt-backed
“恢复原文件” action refuses to restore if the host file or backup changed.

## Model routes and compression bindings

Each binding still owns one Devin model UID and one legacy primary model hash.
Two optional fields extend it without breaking old settings rows:

| Field | Meaning |
| --- | --- |
| `kind` | `standard` (default) or `context_compression` |
| `routes` | Candidate routes; each has `route_id`, `model_hash`, `label`, `enabled` |
| `active_route_id` | The candidate route that currently serves the UID |

Resolution rules:

- A binding with no routes keeps using its legacy `model_hash`, so settings
  written by earlier builds load unchanged and are never rewritten.
- A binding with routes uses the enabled route named by `active_route_id`. If
  that route is missing, disabled, or has an empty hash, the legacy primary hash
  is used instead.
- `AssignModel` still issues its token for the Devin model UID, not for a route
  ID, and the catalog still exposes exactly one Devin UID per binding.
- `kind` is control-plane metadata only. A context-compression binding is an
  ordinary Devin UID mapping at runtime, but it is single-line: it must not
  define candidate routes, and switching a binding to compression clears them.
  That keeps compression configuration independent from ordinary routes.

Validation rejects empty route IDs, duplicate route IDs, empty route hashes, and
an `active_route_id` that does not exist or is disabled. It also rejects
candidate routes on a context-compression binding. Catalog generation and
settings reload never rewrite bindings, so the active route, route order, route
labels, and disabled candidates survive a save or a catalog refresh.

The Devin settings page shows the legacy primary hash as a synthetic `primary`
route until the binding is edited. Adding the first candidate materializes that
route and keeps it active, so adding a route never silently switches the model
that Devin is already using. Route selection is manual: switching the active
route is an explicit user action.

Deliberately out of scope in this phase:

- no automatic retry with another candidate route after a provider failure, and
- no official Devin account routing or upstream catalog fetching.

A streamed provider failure may already have emitted tokens or tool calls, so
automatic fallback needs a verified Devin request lifecycle and a separate
idempotency policy. Official routing needs real black-box Devin captures before
any wire field is invented.

## Verified behavior

The following was verified end to end against a built `cursor-server.exe` with an
isolated data directory (`HAXSD_BYOK_DATA_DIR`), never against the running
desktop product and never against real Devin traffic:

| Check | Result |
| --- | --- |
| Fresh settings default | `enabled=false`, ports `43110/43111/43112`, empty bindings |
| Disabled gateway | no listener on any of the three ports |
| Empty UID/hash binding | rejected with HTTP 400 |
| Duplicate UID bindings | rejected with HTTP 400 |
| Port `0` | rejected with HTTP 400 |
| Enabled gateway after restart | all three ports listen on `127.0.0.1` only |
| `/health` on each port | `{"ok":true,"service":"devin"}` |
| Non-POST on an RPC path | HTTP 405, `Devin gateway accepts POST only` |
| Unknown RPC method | HTTP 404, `unsupported Devin RPC method` |
| Missing or wrong token | Connect end-frame `unauthenticated` |
| `GetCliModelConfigs` | returns the enabled binding's UID, display name and the local API URL |
| `AssignModel` with the mapped UID | returns a fresh UUID session token scoped to the UID |
| `AssignModel` with an unmapped UID | Connect error `invalid_argument`, `Devin AssignModel has no mapped model UID` |

### Streaming round trip

A full `GetChatMessage` round trip was verified with a real model record whose
`base_url` pointed at a local OpenAI-compatible stub that streams fixed text and
a usage block. The request was a hand-built Connect frame carrying a system
prompt, one user message, the mapped model UID, a cascade ID, and an execution
ID.

| Stage | Observed |
| --- | --- |
| Gateway framed response | two message frames carrying `gateway ` and `round trip ok`, then a finish frame, then the end-stream frame |
| Provider request | `POST http://127.0.0.1:<stub>/v1/chat/completions` with `model=stub-model`, the system message, the user message `say hello`, `stream=true` |
| Product call log | `call_id=devin:exec-e2e`, `conversation_id=devin:cascade-e2e`, `model_hash` of the binding's model, `provider_type=openai-chat`, `status=completed`, `finish_reason=stop` |
| Usage accounting | the product recorded the provider's `11` input / `4` output tokens; those numbers are what the gateway folds into its finish frame |

### Tool-call round trip

The same harness was run with a tool definition in the request and a stub that
streams a tool call in pieces (id and name first, argument fragments after):

| Stage | Observed |
| --- | --- |
| Provider request | the Devin tool definition arrived as an OpenAI `tools[0]` entry with `type: function`, the name, the description, and the JSON schema |
| Gateway framed response | a text frame, then a tool-call frame carrying `call_stub_1`, `read_file`, and the fully aggregated `{"path":"src/main.rs"}` |
| Product call log | `finish_reason=tool_use`, `tool_count=1`, `input_tokens=21`, `output_tokens=9` |

A tool call that arrives as several provider deltas is therefore aggregated into
one Devin tool call with its arguments intact, and a Devin tool definition
survives the trip to the provider. Executing the tool stays Devin's job; the
gateway only forwards it.

To re-run the whole thing: start the server with `HAXSD_BYOK_DATA_DIR` pointed at
a throwaway directory, `POST /__byok-api__/api/models` to add a model whose
`base_url` is a local OpenAI-compatible endpoint, `PUT /__byok-api__/api/devin/settings`
to bind that model's hash to a Devin model UID, restart the server, then
`POST http://127.0.0.1:43111/devin.ConnectService/GetChatMessage` with a framed
body and a valid `Authorization: Bearer` header. Nothing about that procedure
touches the desktop product's own data directory.

RPC errors are returned the way the Connect protocol expects: the HTTP status is
`200` and the failure travels in the end-stream frame, while transport-level
failures (wrong method, unknown path) keep their plain HTTP status. A successful
RPC answers a framed request with a framed body plus the end-stream frame,
which is how streaming clients distinguish "no more messages" from transport
failure.

Still not covered: tool-call streaming, thinking/signature replay, and image
parts were exercised only at the unit-test level, and no real Devin client has
been pointed at the gateway yet.

## Intentional limits

The current integration does not automatically discover, edit, or SSH into a
vendor installation. It also does not copy the reference application's
commercial authorization, remote patching, SQLite mutation, or proprietary
status-extension behavior. Unknown host versions fail closed.

This is intentional: the Devin adapter is a separate opt-in path, and the
Cursor listener/harness continues to run independently when Devin is disabled
or when the Devin listener stops.

Automatic updates are intentionally disabled in this product build. Do not
point it at the Cursor BYOK release manifest; create a dedicated `haxsd byok`
release channel before adding updates back.
