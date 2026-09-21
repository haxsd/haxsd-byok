# Devin router: vendor evidence vs this implementation

This note records what the commercial Devin Model Router actually stores on a
machine where it is installed, and where this branch's implementation differs.
It exists because several features were deferred for lack of real evidence; this
is the first real evidence, and it changes what is guesswork and what is not.

Source: the vendor product's own state files under
`%APPDATA%\devin-model-router\` (`route-state.json`, `config.json`) on the
development machine. Only structure, field names, and non-sensitive values are
recorded here. Credentials, license state, tokens and usage data are not copied
into the repository.

## What the vendor stores

`config.json` (version 2) is organised by concern:

| Key | Meaning |
| --- | --- |
| `runtime` | `apiPort`, `inferencePort`, `localApiPort`, `autoStart`, `devinPath` |
| `profiles` | Reusable provider targets: `id`, `name`, `provider`, `baseUrl`, `apiPath`, `apiKey`, `models`, `maxTokens`, `cacheMode`, `responsesTransport` |
| `slots` | Devin-facing model entries: `id`, `uid`, `displayName`, `profileId`, `modelId`, `enabled`, `defaultRouteId` |
| `bindings` | Route records: `id`, `kind`, `slotId`, `devinModelUid`, `devinFamilyUid`, `displayName`, `familyName`, `profileId`, `modelId`, `defaultRouteId`, `enabled`, plus per-kind extras such as `cacheNamespace` and `instructionEnabled` |
| `remoteSsh*` | Remote execution: connection settings plus `autoPatch`, `autoReconnect`, `autoConnect` |

Observed values worth noting:

- The vendor listens on `43100 / 43101 / 43102`. This branch defaults to
  `43110 / 43111 / 43112`, so both can run side by side without a port clash.
- Binding `kind` is not a single flag; three kinds appear in practice:
  - `legacy` — the BYOK compatibility mapping for classic UIDs such as
    `MODEL_CLAUDE_4_SONNET_BYOK`;
  - `auxiliary` — background work; the observed entry is the context-compression
    binding and carries a `cacheNamespace`;
  - `native` — a real Devin model UID such as `deepseek-v4-1-flash-high` grouped
    under a family UID (`deepseek-v4-1-flash`).
- `slots` and `bindings` are separate: a slot is what Devin sees, a binding is
  how it is served. The same `profileId`/`modelId` pair serves several slots.

`route-state.json` (version 2) is the runtime half:

```json
{
  "version": 2,
  "selection": { "family:<family-uid>": "<route-id>" },
  "health": {},
  "configuredDefaults": { "family:<family-uid>": "<route-id>" },
  "lastActiveGroupId": ""
}
```

Three things are worth copying conceptually: selection is keyed **per family**,
not per UID; `configuredDefaults` keeps what the user configured separate from
what is currently selected; and `health` is its own section rather than mixed
into selection.

## Where this branch differs

| Concern | Vendor | This branch |
| --- | --- | --- |
| Provider credentials | Reusable `profiles` | Reuses the existing Cursor BYOK model store; no second credential store |
| Devin-facing entry | `slots` (separate from bindings) | One binding serves as both, keyed by `model_uid` |
| Route records | `bindings` with `kind` and `defaultRouteId` | `routes` per binding with `active_route_id` |
| Selection scope | Per family | Per binding |
| Model families | `devinFamilyUid` / `familyName` | Not modelled |
| Background work | `kind: auxiliary` + `cacheNamespace` | `kind: context_compression`, no cache namespace |
| Runtime health | `health` section in route state | Not modelled |
| Remote execution | `remoteSsh` with auto patch/connect | Explicit local path only, never automatic |

## The host file on a real installation

The vendor patches the same file this branch's host-patch boundary targets:

```
D:\devin\Devin\resources\app\extensions\windsurf\dist\extension.js
```

Its own backup sits next to it as `extension.js.devin-model-router.backup`
(9 739 343 bytes; the live file is 9 739 096 bytes — the difference is the longer
`getConfig(...)` calls the vendor replaced with literal URLs).

Running this branch's `inspect` against copies of both files gives:

| File | `compatible` | `clean` | `patched` | `ports` |
| --- | --- | --- | --- | --- |
| the vendor's backup | true | **true** | false | none |
| the live file (vendor-patched) | true | false | **true** | `43100 / 43101 / 43102` |

Three consequences:

1. The four anchors this branch looks for do occur in a real Devin build, in the
   shape the patcher expects, so the anchor model is not a guess.
2. `apply` refuses a vendor-patched file with
   `Devin host file is not a known clean version`. That is the intended
   fail-closed behaviour, but it means switching from the vendor router to this
   gateway needs the clean file first. The vendor's own backup is exactly that,
   so the route is: point the status check at the backup, confirm `clean`,
   restore it over the live file, then apply this branch's patch.
3. On this machine the vendor router is installed but not running while Devin is
   patched to `127.0.0.1:43100`, which is why this switch matters in practice
   and not only in theory.

### Why the endpoint cannot be redirected by configuration

Devin also declares settings keys for this, so a configuration route looks
tempting at first:

```
codeium.apiServerUrl            (Config.API_SERVER_URL)
codeium.registerApiServerUrl
codeium.inferenceApiServerUrl   (Config.INFERENCE_API_SERVER_URL)
```

The live bundle resolves the endpoint through a different function, and the
vendor's patch is exactly what changed it. The two forms side by side:

```js
// the clean backup: the endpoint is read from configuration
getApiServerUrl = A => getConfig(Config.API_SERVER_URL) !== DEFAULT_API_SERVER_URL
                       || isEmpty(A) ? getConfig(Config.API_SERVER_URL) : A
async restart(A) { this.apiServerUrl = A; ... }        // takes the URL it is given

// the live, vendor-patched file: the endpoint is a literal
getApiServerUrlFromContext = A => "http://127.0.0.1:43100"
async restart(A) { A = "http://127.0.0.1:43100"; ... } // ignores what it is given
```

`getApiServerUrlFromContext` is what the extension calls before
`LanguageServerClient.initialize`, and no environment variable overrides it
(`process.env` carries no match for it). On a patched installation the setting
keys are therefore a dead end: writing `codeium.apiServerUrl` into
`%APPDATA%\Devin\User\settings.json` will not move the endpoint, and a runbook
must not suggest it. Patching the file is the working mechanism, so the swap
sequence below is required rather than optional.

### The switch was exercised on a copy of that real file

Running this branch's endpoints against copies only — the live installation was
never touched — produced a complete cycle:

| Step | Result |
| --- | --- |
| Inspect the vendor backup | `clean=true`, no ports |
| Apply this branch's patch to it | receipt with `original_sha256` of the clean file and `patched_sha256` of the result; a SHA-256-verified backup was written first |
| Inspect patched result | `patched=true`, ports `43110 / 43111 / 43112` |
| Patched content | 2 × `http://127.0.0.1:43110`, 1 × `43111`, 1 × `43112`; the official `getConfig(...)` calls it replaced are gone |
| Restore | `{"restored":true}`, and the file's SHA-256 returned to the vendor backup's exact value |

So the procedure for switching an existing installation from the vendor router to
this gateway is:

1. copy the vendor backup to a scratch path and make that path the subject of
   every step, so a mistake cannot damage the installation, for example
   `Copy-Item <windsurf dist>\extension.js.devin-model-router.backup <scratch>\extension.clean.js`;
2. point the status check at that copy and confirm it reports `clean`;
3. apply this branch's patch to the copy and confirm the reported ports;
4. only then repeat the same two steps against the live file and restart Devin.

The opposite direction works the same way, because this branch's patch writes its
own backup next to the file.

## What is still not proven

The vendor's **wire** behaviour — which Devin RPC fields carry a family UID,
how an auxiliary request is distinguished from a chat request, and how the
vendor advertises native model UIDs in the catalog — is still unobserved. The
files above are its control plane, not its protocol. Any change to this branch's
Connect adapter therefore still needs a real capture from a Devin client talking
to the gateway.

What the evidence does settle: family-level selection, a separate configured
default, a `health` section, and a context-compression binding with its own
cache namespace are real vendor concepts, not speculations. Implementing any of
them is now a design decision with a reference, which is a different position
from the one recorded when those features were deferred.
