# Branch isolation

This repository intentionally contains two independent product lines. They are
not two stages of one release and must not be merged into each other.

| Branch | Product | Scope | Default data directory |
| --- | --- | --- | --- |
| `main` | Cursor BYOK | Cursor-only product line | `.cursor-byok-v3` |
| `feat/devin-router` | `haxsd byok` | Devin-compatible product line with its own Cursor model harness | `.haxsd-byok-devin-v3` |

## Non-negotiable rules

1. Do not merge `feat/devin-router` into `main`.
2. Do not rebase or cherry-pick Devin product code into `main` unless the owner
   explicitly requests a new, separately reviewed integration.
3. Do not release the two products from the same tag or updater channel.
4. Keep the product identifier, executable name, data directory, database, and
   local-storage keys separate.
5. Before changing or pushing code, verify the current branch with
   `git branch --show-current` and confirm that the worktree is the intended
   product worktree.

The Devin product currently has automatic updates disabled. A future update
channel must be created specifically for `haxsd byok`; it must never point to
the Cursor BYOK `main` release manifest.

## Local checkouts

The two product lines live in two separate clones of this repository, not in one
clone with two worktrees:

| Product | Checkout | Branch | Remote |
| --- | --- | --- | --- |
| `haxsd byok` (Devin) | `D:\cursor-byok\byok-dev\cursor-byok-devin-router` | `feat/devin-router` | `haxsd/haxsd-byok` (product home) |
| Cursor BYOK | `D:\cursor-byok\byok-dev\cursor-byok-upstream` | `main` | `leookun/cursor-byok` |

The Cursor checkout tracks the upstream repository itself, so it must not receive
Devin commits and must not push product changes back to upstream.

If a task concerns Devin, work only in the Devin checkout and push only
`feat/devin-router`. If a task concerns the Cursor product, work only in the
Cursor checkout and keep the Devin branch untouched.
