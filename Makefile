.PHONY: check dev-web dev-server dev-desktop build-web build-server build-desktop build-docker

check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace --all-targets
	npm --prefix apps/desktop run check

dev-web:
	npm --prefix apps/desktop run dev:web

dev-server:
	CURSOR_CONSOLE_DIR=apps/desktop/dist cargo run --package cursor-server --bin cursor-server

dev-desktop:
	npm --prefix apps/desktop run tauri:dev

build-web:
	npm --prefix apps/desktop run build

build-server:
	cargo build --release --package cursor-server --bin cursor-server

ifeq ($(OS),Windows_NT)
build-desktop:
	@node -e "const { spawnSync } = require('node:child_process'); const result = spawnSync(process.execPath, ['node_modules/@tauri-apps/cli/tauri.js', 'build', '--bundles', 'nsis'], { cwd: 'apps/desktop', stdio: 'inherit' }); process.exit(result.status ?? 1)"
else
build-desktop:
	npm --prefix apps/desktop run tauri:build
endif

build-docker:
	docker build --tag haxsd-byok:local .
