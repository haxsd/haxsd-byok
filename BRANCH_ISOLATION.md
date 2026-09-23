# Repository isolation

Two independent products live in two separate repositories. They share a common
ancestor (upstream `leookun/cursor-byok`) but are separate applications with
separate identities, data directories and release channels. They are not two
stages of one release and must not be merged into each other.

| Product | Repository | Branch | Scope | Default data directory |
| --- | --- | --- | --- | --- |
| `haxsd byok` | `haxsd/haxsd-byok` (this repository) | `main` | Devin-compatible product line with its own Cursor model harness | `.haxsd-byok-devin-v3` |
| Cursor BYOK | `haxsd/cursor-byok` | `main` | Cursor-only product line | `.cursor-byok-v3` |

## Non-negotiable rules

1. Do not port code from one product to the other. A change that affects both is
   two changes: it is reviewed and landed once per repository.
2. Keep the product identifier, executable name, data directory, database and
   local-storage keys separate: `dev.haxsd.byok` here, `dev.cursorbyok.desktop`
   in the Cursor product.
3. Do not release both products from the same tag. This repository's release tags
   are prefixed `haxsd-byok-v*`; the Cursor product uses `v*`.
4. Never point one product's updater endpoint at the other product's release
   manifest, and never reuse the other product's signing key.
5. Before changing or pushing code, confirm the repository, branch and worktree:
   `git remote -v` and `git branch --show-current`.
6. Upstream changes are ported by hand, file by file. Both product repositories
   were re-imported from upstream, so their histories share no ancestor with
   `leookun/cursor-byok` and `git merge upstream/main` is not an option.

## Release channels

| Product | Updater endpoint | Signing key |
| --- | --- | --- |
| `haxsd byok` | `haxsd/haxsd-byok` releases, tags `haxsd-byok-v*` | public key `5B1D4F333875D202`; private key in the `TAURI_SIGNING_PRIVATE_KEY` secret and in `D:\cursor-byok\byok-dev\.updater-keys\haxsd-byok.key` |
| Cursor BYOK | `haxsd/cursor-byok` releases, tags `v*` | public key `67DF32FA5EB7A316`; private key only in that repository's `TAURI_SIGNING_PRIVATE_KEY` secret |

## Local checkouts

| Product | Checkout | Branch | Remote |
| --- | --- | --- | --- |
| `haxsd byok` | `D:\cursor-byok\byok-dev\haxsd-byok` | `main` | `haxsd/haxsd-byok` |
| Cursor BYOK | `D:\cursor-byok\byok-dev\cursor-byok-product` | `main` | `haxsd/cursor-byok` |
| Upstream reference | `D:\cursor-byok\byok-dev\cursor-byok-upstream` | `main` | `leookun/cursor-byok` |

The upstream reference checkout exists to read upstream code and to compute its
diffs; it must not receive product commits and must not push to upstream.

## Retired: `feat/devin-router`

`haxsd byok` was first developed as `feat/devin-router` inside
`haxsd/cursor-byok`. That arrangement ended: the branch was archived as
`legacy/devin-router` (kept read-only; the per-file port checklist against this
repository lives in `D:\cursor-byok\byok-dev\_logs\devin-legacy-inventory.md`)
and was removed from the remote branch list. All `haxsd byok` work happens in
this repository's `main`.
