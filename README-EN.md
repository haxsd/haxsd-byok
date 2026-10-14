<div align="center">

# haxsd byok

Run Cursor and Devin on model APIs you configure: a local model gateway with a model library, connection status and call records.

[Download for Windows](https://github.com/haxsd/haxsd-byok/releases/latest) · [User guide](./docs/user-guide.md) · [Devin setup](./docs/devin-go-live.md) · [Troubleshooting](./docs/troubleshooting.md) · [中文](./README.md)

</div>

## Features

- **Model library**: base URL, API key and model ID; OpenAI Chat / Responses and Anthropic; built-in connection test.
- **Cursor integration**: official, local or mixed model sources, with a local proxy, local CA and takeover status.
- **Devin integration**: optional local gateway, model bindings and a host patch.
- **Call insights and value estimate**: success, latency and token usage, with an estimate from reference prices.
- **Desktop app**: Chinese and English, multiple themes, tray, in-app tutorial and signed updates.

Model services are yours to provide. Keys and settings stay on this machine; requests go to the providers you configure.

## Getting started

1. Download the installer whose name contains `x64-setup.exe` from [Releases](https://github.com/haxsd/haxsd-byok/releases/latest) and install it.
2. On the **Models** page, add base URL, API key and model ID, pick the protocol, and use **Test** to confirm it works.
3. For Cursor: initialize the local CA and trust it, then pick the model source on the **Cursor** page. **Enabling local or mixed mode stops a running Cursor, so save your work first**; choose **mixed** to keep official models available alongside yours.
4. For Devin: set up model mappings, enable the gateway and save, restart the app, then follow [Devin setup](./docs/devin-go-live.md).

Full steps for URLs, upgrades, backup and restore are in the [user guide](./docs/user-guide.md).

## Downloads and updates

- **Windows x64 installer** (NSIS) only for now; check for updates in **Settings → Software update**.
- Updates come from this repository and are verified with this product's own Tauri update signature. That is not a Windows code signature, so SmartScreen may warn about the publisher: choose **More info → Run anyway**.
- Data lives in `%USERPROFILE%\.haxsd-byok-devin-v3` (database, certificate, runtime data). API keys are stored in the local database, so treat backups as sensitive.

## Boundaries

- Local and mixed modes rewrite Cursor's proxy configuration, so restart Cursor when you switch modes; official mode writes nothing and records nothing.
- Local mode only knows the models in your library; anything else is refused instead of silently going official.
- Devin is off by default and listens on `127.0.0.1` only; after changing ports, restart the app and re-apply the host patch.
- This product keeps separate data, identifiers and update channels from [haxsd/cursor-byok](https://github.com/haxsd/cursor-byok).

## Support and license

Check [troubleshooting](./docs/troubleshooting.md) first, then open an [issue](https://github.com/haxsd/haxsd-byok/issues). Include the version and the steps; never upload API keys, the CA private key or a full database.

Based on [leookun/cursor-byok](https://github.com/leookun/cursor-byok), under the [MIT license](./LICENSE).
