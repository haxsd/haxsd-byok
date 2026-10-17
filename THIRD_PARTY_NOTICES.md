# Third-party notices

## OpenAI Codex base instructions

`server/prompt/codex/base.md` is derived from `codex-rs/models-manager/prompt.md`
in [openai/codex](https://github.com/openai/codex), commit
`092d3acd6bec3e3a14bdc7e7a2810ab628ab759d` (`rust-v0.162.1`).

Copyright 2025 OpenAI. Licensed under the Apache License, Version 2.0;
the full license is included at `server/prompt/codex/LICENSE`.
The local copy normalizes formatting and includes a source attribution comment. These instructions are supplied to the
original Codex client as model-catalog data; they are not instructions to the
haxsd-byok server or its contributors.

## CC Switch design reference

The Codex integration's protocol design was informed by the Responses/Chat and
Responses/Anthropic adapters in [farion1231/cc-switch](https://github.com/farion1231/cc-switch),
commit `51ac86f1c59abfefabadebd05ec5a5415b8ad72c`.
CC Switch is Copyright (c) 2025 Jason Young, MIT licensed.
The local adapters are independent implementations; no CC Switch runtime,
configuration database, history migration, or opaque-state format is bundled.

## WebView2Loader.dll

`apps/desktop/src-tauri/webview2/WebView2Loader.dll` is the loader library from
Microsoft's [Edge WebView2 SDK](https://learn.microsoft.com/microsoft-edge/webview2/).
It is committed and shipped as an application resource
(`apps/desktop/src-tauri/tauri.conf.json`, `bundle.resources`) so the packaged
Windows build can create the WebView2 environment on a machine that has no
separate SDK install. SHA-256:
`8427b1fc58ec707813e5c0a51eb5d69397bb333250a7b891be4d3b123f1e0f1c`.

It is a Microsoft redistributable component governed by the Microsoft Software
License Terms for the WebView2 SDK, not by this repository's MIT license. The
terms travel with the SDK; no Microsoft source code is otherwise copied here.

## potion-code-16M-v2 embedding model

Semble's semantic channel uses Minish Lab's
[`minishlab/potion-code-16M-v2`](https://huggingface.co/minishlab/potion-code-16M-v2)
(`model.safetensors`, `tokenizer.json`). `crates/semble-core/src/embedding/assets.rs`
downloads those two files on first use instead of bundling them, and pins each one
by SHA-256 so an unexpected artifact is rejected rather than loaded.

The model is the property of its authors; the terms published on the model card
apply to it and are not superseded by this repository's MIT license. The Rust
implementation around it is an independent reimplementation of the MIT-licensed
Semble upstream — see `UPSTREAM.md` for the pinned baseline commit.
