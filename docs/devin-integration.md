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
- Optional, manually selected candidate routes per binding. Custom models
  appear directly in Devin’s native picker, including supported parameter variants;
  the official model catalog is preserved.
- The existing provider router and Devin tool definitions are reused. Devin
  remains responsible for executing its tools; this product supplies model
  inference and forwards model events.
- A structure-detected compatibility patch for Devin `extension.js`.
  Packaged Devin only reads `codeium.apiServerUrl` and
  `codeium.inferenceApiServerUrl` when its development extension is installed,
  so the patch removes exactly that one gate. It embeds no URL, creates
  SHA-256-verified backups of both the extension and settings file, replaces
  atomically, and refuses missing, ambiguous or inconsistent endpoint-settings gates.

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

The patch identifies exactly one endpoint-settings gate using its code structure
and surrounding settings anchors; it is not pinned to a version or prepublished
extension hash. Missing or ambiguous structures fail closed. SHA-256 verifies
backups and restore receipts, and restore refuses files changed after this
application wrote them.

This patch changes where an already signed-in Devin client sends its model
requests. It does not create an account, bypass a login, or change the plan
returned by Devin's official service. The ordinary Devin account still uses its
official session; the local gateway only answers the model methods this product
owns.

## Native models and routes

Choose “添加自选模型” to add a model-library entry to Devin’s native picker under
its library name, or “添加 Devin 模型” to map an existing Devin model UID. Custom
entries use reserved `haxsd-byok:` identities and expose supported parameter
variants. The gateway appends them to official catalog and user-status responses;
official model requests without a local mapping continue upstream. Login,
account plans and quota remain controlled by Devin.

Each binding selects a model hash and may have candidate `routes` plus an
`active_route_id`. Without an active route it uses the primary model; selecting
a route is manual, and provider failure never silently switches to another route.
Route IDs must be nonempty and unique within a binding, model hashes must be
nonempty, and an active route must exist and be enabled. `default_model_uid` must select an enabled
binding before the gateway can be enabled. Catalog reads do not rewrite bindings.
The former `kind` / `context_compression` binding setting has been removed.

Native model directory, assignment and session state are handled by the gateway;
non-owned methods and official model requests are forwarded upstream. Local
routing uses the selected model’s existing provider adapter; Devin still executes
its own tools.

Normal application exit preserves the Devin integration files but stops the
listeners. Restart haxsd byok before using local models again; explicitly restore
the Devin settings to stop using this integration.

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

The current integration discovers the local client and patches only a uniquely
recognized endpoint-settings gate; it does not support remote installs or SSH,
and does not guarantee compatibility with arbitrary client builds. It does not
copy the reference application’s commercial authorization or mutate Devin’s
SQLite data. Missing or ambiguous endpoint-settings structures fail closed.

This is intentional: the Devin adapter is a separate opt-in path, and the
Cursor listener/harness continues to run independently when Devin is disabled
or when the Devin listener stops.

Updates ship through this product's own release channel. `release.yml` builds
the installer, signs the Tauri update, and publishes `latest.json` for tags such
as `haxsd-byok-v<version>`; the in-app updater reads that manifest. Do not point
the updater at the upstream Cursor BYOK release channel: the two products have
separate identifiers, data directories, and signing keys.
