# Devin integration

`haxsd byok` is the product on this repository's `main` line. It combines four
client paths — Cursor, Devin, Codex, and Claude — on one shared model library
and model harness. The Devin-compatible gateway described here is optional, has
its own listeners, and is disabled by default.

The product uses its own Tauri identifier (`dev.haxsd.byok`) and storage
directory (`.haxsd-byok-devin-v3`). It does not share the upstream Cursor BYOK
release channel or data directory.

## What is integrated

- Devin Connect request framing, gzip, protobuf-like fields, streaming text,
  thinking, tool calls, signatures, stop reasons, and usage.
- Devin model catalog responses backed by the shared model library.
- `AssignModel` sessions with opaque 12-hour tokens. The implementation does
  not copy the reference router's commercial license or JWT behavior.
- Devin model UID to model-hash bindings, persisted in the existing settings
  store. API keys remain in the existing model records.
- Optional candidate routes per binding and a binding kind that marks a Devin
  UID as a context-compression binding.
- The existing provider router and Devin tool definitions are reused. Devin
  remains responsible for executing its tools; this product supplies model
  inference and forwards model events.
- A version-locked compatibility patch for Devin `3.11.216` `extension.js`.
  Packaged Devin only reads `codeium.apiServerUrl` and
  `codeium.inferenceApiServerUrl` when its development extension is installed,
  so the patch removes exactly that one gate. It embeds no URL, creates
  SHA-256-verified backups of both the extension and settings file, replaces
  atomically, and refuses every unknown or partially patched bundle.

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

The settings page detects the installed Devin data directory and `extension.js`
automatically. If Devin lives in a custom location, enter either its installation
directory or the `extension.js` file itself under “Advanced”. The page remembers
both the settings path (`devin_host_path`) and the installation root
(`devin_install_root`) in the desktop settings, and uses them for host status,
apply, restore, and model UID discovery. Supported layouts under an installation
root are `resources/app/extensions/windsurf/dist/extension.js`,
`Contents/Resources/app/...`, `Resources/app/...`, and `app/...`. A pasted path
must exist; an invalid path fails with an explanation instead of silently using a
different installation. Scripts and tests can still override detection with
`HAXSD_BYOK_DEVIN_ROOT`, `HAXSD_BYOK_DEVIN_PATH`, and
`HAXSD_BYOK_DEVIN_SETTINGS`.

Quit Devin completely, then choose “写入接入配置”. The action preflights both
files, patches the extension first, writes the two settings second, and rolls the
extension back if either write fails. A successful action shows both the settings
and extension patch as applied.

The patch supports only the extension hash published in the source constant for
Devin `3.11.216`. A client update, another router's modification, or a partial
patch fails closed. The receipt-backed “恢复设置” action restores either side
independently and refuses to overwrite a file changed after this application
wrote it.

This patch changes where an already signed-in Devin client sends its model
requests. It does not create an account, bypass a login, or change the plan
returned by Devin's official service. The ordinary Devin account still uses its
official session; the local gateway only answers the model methods this product
owns.

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
  ID, and the catalog still exposes exactly one Devin UID per binding. It is
  answered locally together with `GetChatMessage` and `GetCliModelConfigs`;
  every other method is forwarded to the Devin service untouched.
- `kind` is control-plane metadata only. A context-compression binding is an
  ordinary Devin UID mapping at runtime, but it is single-line: it must not
  define candidate routes, and switching a binding to compression clears them.
  That keeps compression configuration independent from ordinary routes.

Validation rejects empty route IDs, duplicate route IDs, empty route hashes, and
an `active_route_id` that does not exist or is disabled. It also rejects
candidate routes on a context-compression binding. `default_model_uid` must
name an enabled standard binding; saving an enabled gateway also requires one.
Catalog generation and settings reload never rewrite bindings, so the active
route, route order, route labels, and disabled candidates survive a save or a
catalog refresh.

The Devin settings page shows the legacy primary hash as a synthetic `primary`
route until the binding is edited. Adding the first candidate keeps the primary
model in use and leaves the active route unset, so adding a route never silently
switches the model Devin is already using. Route selection is manual: switching
the active route is an explicit user action.

The default model is also explicit. `default_model_uid` selects which Devin-side
mapping is marked default in the catalog and which UID the Cascade/CLI
default-override field names. Without it, no model is marked default and the
page asks the user to choose one instead of promoting the first enabled binding.

Deliberately out of scope in this phase:

- no automatic retry with another candidate route after a provider failure, and
- no official Devin account routing or upstream catalog fetching.

A streamed provider failure may already have emitted tokens or tool calls, so
automatic fallback needs a verified Devin request lifecycle and a separate
idempotency policy. Official routing needs real black-box Devin captures before
any wire field is invented.

## Verified behavior

The following was verified against a built `cursor-server.exe` with an isolated
data directory (`HAXSD_BYOK_DATA_DIR`) and a real Devin `3.11.216` client. The
running desktop product's data directory was not opened or changed.

| Check | Result |
| --- | --- |
| Fresh settings default | `enabled=false`, ports `43110/43111/43112`, empty bindings, no default model |
| Disabled gateway | no listener on any of the three ports |
| Empty UID/hash binding | rejected with HTTP 400 |
| Duplicate UID bindings | rejected with HTTP 400 |
| Port `0` | rejected with HTTP 400 |
| Enabled gateway after restart | all three ports listen on `127.0.0.1` only |
| `/health` on each port | `{"ok":true,"service":"devin"}` |
| Non-POST on an RPC path | HTTP 405, `Devin gateway accepts POST only` |
| Unknown RPC method | forwarded to the Devin service untouched (sign-in, account state and telemetry keep reaching Devin) |
| Missing or wrong token | Connect end-frame `unauthenticated` |
| `GetCliModelConfigs` | returns the enabled binding's UID, display name and the local API URL |
| `AssignModel` with the mapped UID | returns a fresh UUID session token scoped to the UID |
| `AssignModel` with an unmapped UID | Connect error `invalid_argument`, `Devin AssignModel has no mapped model UID` |
| Current installed `extension.js` copy | real 7 MB file patched and restored; SHA-256 returned to `A063DAE8…FF0` |
| Settings plus extension apply | extension gate written first, both settings written second, status requires both |
| Real Devin language server | normal launch shows `--api_server_url http://127.0.0.1:44110 --inference_api_server_url http://127.0.0.1:44111` |
| Real Devin model request | mapped `swe-1-6-slow` returned `gateway round trip ok`; call count changed from 2 to 3 |
| Real request compression | `Content-Encoding` absent, `Connect-Content-Encoding: gzip`, envelope flag `1`, payload gzip; handled without double decompression |

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

Tool-call streaming, thinking/signature replay, and image parts have only been
exercised at the unit-test level. A real text request and the real model catalog
have been verified through Devin `3.11.216`.

### Account boundary

Devin's official account controls still apply. During verification a fresh
session displayed “You ran out of free credits. Please subscribe to continue
using Devin”. The existing already-created session could still be used and its
`SWE-1.6 Slow` request was answered locally, but this product does not patch,
fake, or bypass the account quota, plan, login, or new-session entitlement.
Accounts with available official access keep that access; BYOK only changes
where the model request goes.

## Intentional limits

The current integration discovers the local client and patches one exact
extension version; it does not support remote installs, SSH, or arbitrary host
versions. It also does not copy the reference application's commercial
authorization, SQLite mutation, or proprietary status-extension behavior.
Unknown extension hashes fail closed.

This is intentional: the Devin adapter is a separate opt-in path, and the
Cursor listener/harness continues to run independently when Devin is disabled
or when the Devin listener stops.

Updates ship through this product's own release channel. `release.yml` builds
the installer, signs the Tauri update, and publishes `latest.json` for tags such
as `haxsd-byok-v<version>`; the in-app updater reads that manifest. Do not point
the updater at the upstream Cursor BYOK release channel: the two products have
separate identifiers, data directories, and signing keys.
