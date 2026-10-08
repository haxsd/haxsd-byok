<div align="center">

# haxsd byok

Use your configured model APIs in Cursor and Devin, with local model management, connection status and call records.

[Download for Windows](https://github.com/haxsd/haxsd-byok/releases/latest) · [中文 / Setup guide](./docs/user-guide.md) · [Devin setup](./docs/devin-go-live.md) · [Troubleshooting](./docs/troubleshooting.md)

</div>

## Features

| Feature | What it does |
| --- | --- |
| Model library | Configure endpoints, keys and model IDs for OpenAI Chat / Responses and Anthropic; add, copy, reorder and test models |
| Cursor integration | Local proxy and CA, with certificate, ownership and takeover status |
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
3. For Cursor, initialize the local CA, follow trust instructions and enable takeover. **Save your work first: enabling takeover terminates running Cursor processes. Reopen Cursor manually.**
4. For Devin, configure bindings, enable and save the gateway, restart haxsd byok, apply the [host integration](./docs/devin-go-live.md), then restart Devin.
5. Send a message and verify its record on the Calls page. Configure only the integrations you use.

The [user guide](./docs/user-guide.md) covers endpoints, updates, backups and restoring client configuration. Detailed guides are currently in Chinese.

## Downloads and updates

Official releases currently provide a **Windows x64 NSIS installer**. Other platforms have source and CI checks, but no macOS/Linux release installers yet.

- Use System settings → Software update to check and install.
- For manual updates, exit the app and run the newer installer. It reuses the registered installation directory; data is separate.
- Updates use this repository's manifest, installer and independent Tauri signing key.
- Tauri update signatures are distinct from Windows Authenticode signing. Windows may still display a source prompt.

**1.0.19** removes the cumulative call count from the top-bar status pill. Call counts are range-based and live on the dashboard's "LLM calls" tile and the Calls page. [Release notes](https://github.com/haxsd/haxsd-byok/releases/tag/haxsd-byok-v1.0.19)

## Boundaries and data

- Keep the app running while integrated. To stop, disable Cursor takeover, or restore Devin's host file before disabling its gateway.
- Devin is off by default and listens on `127.0.0.1` only, with ports `43110/43111/43112`. Port changes require restart and reapplying the host patch.
- Candidate routes are switched manually, without automatic provider fallback. Host patching accepts recognized versions; recheck after client updates.
- Data defaults to `%USERPROFILE%\.haxsd-byok-devin-v3`. Keys are stored in the local database; protect backups as sensitive files.
- Detailed payloads and inactive runtime state expire; lifetime usage statistics remain. [Retention rules](./docs/user-guide.md#记录与数据保留)
- This is independent of [haxsd/cursor-byok](https://github.com/haxsd/cursor-byok), with separate identity, data and updates. [Isolation rules](./BRANCH_ISOLATION.md)

## Development and support

See the [development guide](./docs/development.md) for structure, build, validation and publishing, [Devin integration](./docs/devin-integration.md) for protocol evidence, and the [1.0.18 quality review](./docs/quality-review-2026-09-28.md) for confirmed issues and recommendations.

For [issues](https://github.com/haxsd/haxsd-byok/issues), include versions, reproduction steps and redacted errors. Do not upload keys, CA private keys or entire databases.

## Credits and license

Based on [leookun/cursor-byok](https://github.com/leookun/cursor-byok), under the [MIT License](./LICENSE). Thanks to the upstream author and contributors. This product adds independent updates, Devin integration, hourly estimates and UI changes, and removes advertising entry points. Use this repository's guides and in-app tutorial for this product.
