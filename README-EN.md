<div align="center">

# haxsd byok

Use your configured model APIs in Cursor and Devin, with local model management, connection status and call records.

[Download for Windows](https://github.com/haxsd/haxsd-byok/releases/latest) · [中文 / Setup guide](./docs/user-guide.md) · [Devin setup](./docs/devin-go-live.md) · [Troubleshooting](./docs/troubleshooting.md)

</div>

## Features

| Feature | What it does |
| --- | --- |
| Model library | Configure endpoints, keys and model IDs for OpenAI Chat / Responses and Anthropic; add, copy, reorder and test models |
| Cursor integration | Three model sources: **official** (this app stays out), **local** (model library only, unknown models are rejected) and **mixed** (vendor models and your own models side by side), plus the local proxy, CA and takeover status |
| Devin integration | Optional loopback gateway, model UID bindings, manual candidate routes and a verified host patch with backup |
| Call records | Success/failure, duration and token usage, with configurable payload recording |
| Value estimates | Editable fixed or peak/off-peak reference prices; independent CNY and USD settings |
| Desktop app | Simplified Chinese / English, themes, tray, in-app tutorial and signed updates |

```text
Cursor → local proxy + CA ─┐
                           ├→ model library → your model API
Devin  → optional gateway ─┘         └→ local call records and statistics
```

You supply the model service. Settings and keys are local; requests go to your configured provider. Cost estimates use saved reference prices and are not provider invoices.

## Getting started

1. Install the Windows asset ending in `x64-setup.exe` from [Releases](https://github.com/haxsd/haxsd-byok/releases/latest).
2. Add an endpoint, key and model ID on the Models page. Select the correct protocol. Test sends a real model request.
3. For Cursor, initialize the local CA, follow the trust instructions, then pick a model source on the Cursor page. **Save your work first: enabling local or mixed mode closes running Cursor processes. Reopen Cursor manually.** To keep the vendor's models alongside your own, pick mixed and sign in to your own account inside Cursor.
4. For Devin, configure bindings, enable and save the gateway, restart haxsd byok, apply the [host integration](./docs/devin-go-live.md), then restart Devin.
5. Send a message and verify its record on the Calls page. Configure only the integrations you use.

The [user guide](./docs/user-guide.md) covers endpoints, updates, backups and restoring client configuration. Detailed guides are currently in Chinese.

## Downloads and updates

Official releases currently provide a **Windows x64 NSIS installer**. Other platforms have source and CI checks, but no macOS/Linux release installers yet.

- Use System settings → Software update to check and install.
- For manual updates, exit the app and run the newer installer. It reuses the registered installation directory; data is separate.
- Updates use this repository's manifest, installer and independent Tauri signing key.
- The Tauri update signature validates the update package; it is not a Windows Authenticode signature. This is a personal project without a purchased code-signing certificate (the upstream project ships unsigned too), so Windows may still show a source prompt.
  - SmartScreen ("Windows protected your PC"): choose **More info** → **Run anyway**.
  - If **Smart App Control** is on (Settings → Privacy & security → Windows Security → App & browser control), unsigned installers are blocked with no "Run anyway": turn Smart App Control off first, then install or update (turning it off is one-way — re-enabling requires resetting Windows).

**1.1.0 milestone** (it carries everything since 1.0.22):

- **Compaction reworked**: a compacted turn no longer collapses to a single summary. The model gets a short handover state plus the most recent turns replayed verbatim, whole turn by turn, for roughly 20% of the context window, so it keeps working on the same file and keeps the constraints it just verified.
- **Compaction fallback**: if the provider still refuses the result as over-limit, the app compacts again with the summary alone — the exact shape older versions sent — so the worst case matches the previous behaviour instead of failing the turn. A failed summarizer call now writes a stated slot plus the recent turns instead of a truncated JSON tail.
- **DeepSeek and other OpenAI Chat 400s** (1.0.22): assistant turns that are only reasoning with no text and no tool calls, and empty Parts content, are omitted when the request is built.
- When compaction triggers (90% of the window, or a provider over-limit error), `/summarize` on short conversations, and models without a configured window all behave exactly as before. Existing conversations need no action. [1.1.0 release notes](https://github.com/haxsd/haxsd-byok/releases/tag/haxsd-byok-v1.1.0)

## Boundaries and data

- Keep the app running in local and mixed modes; both rewrite Cursor's proxy configuration, and crossing the official boundary in either direction requires restarting Cursor. Official mode writes no configuration, injects no account and produces no call records or compaction.
- The Cursor page shows the current mode, the account the client is actually signed in with, and the state of the vendor model catalog (failures are reported there — a 401 means the client needs to sign in again).
- Compaction applies only to requests answered by your own models; the vendor's models manage their own context.
- Local mode knows only the models in your library; a model that is not there is rejected instead of quietly going to the vendor.
- Devin is off by default and listens on `127.0.0.1` only, with ports `43110/43111/43112`. Port changes require restart and reapplying the host patch.
- Candidate routes are switched manually, without automatic provider fallback. Host patching accepts recognized versions; recheck after client updates.
- Data defaults to `%USERPROFILE%\.haxsd-byok-devin-v3`. Keys are stored in the local database; protect backups as sensitive files.
- Detailed payloads and inactive runtime state expire; lifetime usage statistics remain. [Retention rules](./docs/user-guide.md#记录与数据保留)
- This is independent of [haxsd/cursor-byok](https://github.com/haxsd/cursor-byok), with separate identity, data and updates.

## Development and support

See the [development guide](./docs/development.md) for structure, build, validation and publishing, and [Devin integration](./docs/devin-integration.md) for protocol evidence.

For [issues](https://github.com/haxsd/haxsd-byok/issues), include versions, reproduction steps and redacted errors. Do not upload keys, CA private keys or entire databases.

## Credits and license

Based on [leookun/cursor-byok](https://github.com/leookun/cursor-byok), under the [MIT License](./LICENSE). Thanks to the upstream author and contributors. This product adds independent updates, Devin integration, hourly estimates and UI changes, and removes advertising entry points. Use this repository's guides and in-app tutorial for this product.
