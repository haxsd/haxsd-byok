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
