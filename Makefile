.PHONY: check dev-web dev-server dev-desktop build-web build-server build-desktop build-docker bench-check bench-gates

check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace --all-targets
	npm --prefix apps/desktop run check

# 完整跑分：重新索引 React/Vue 固定提交并和本机 codegraph 对比，任一 Semble 轨道低于
# support/benchmarks/semble/quality-gates.json 的下限就以非零退出。需要 codegraph 在
# PATH 上并且能联网拉固定提交，以十分钟计，所以不进 CI。
bench-check:
	cargo run --release --locked -p semble-benchmark -- --check

# 质量门槛的可判定子集（CI 每次跑）：用提交的 results/latest.json 复核同一个门槛文件。
# 不需要 codegraph、模型与网络，因此它拦的是"门槛被调高/报告被改坏"这类改动。
bench-gates:
	cargo run --locked -p semble-benchmark -- --verify-report support/benchmarks/semble/results/latest.json

dev-web:
	npm --prefix apps/desktop run dev:web

# Windows 的 cmd 不认 `VAR=value command` 这种前缀（整串会被当成命令名，报
# "CURSOR_CONSOLE_DIR 不是内部或外部命令"），所以照 build-desktop 的做法用 node
# 注入环境变量并原样转发退出码。
ifeq ($(OS),Windows_NT)
dev-server:
	@node -e "const { spawnSync } = require('node:child_process'); const result = spawnSync('cargo.exe', ['run', '--package', 'cursor-server', '--bin', 'cursor-server'], { stdio: 'inherit', env: { ...process.env, CURSOR_CONSOLE_DIR: 'apps/desktop/dist' } }); process.exit(result.status ?? 1)"
else
dev-server:
	CURSOR_CONSOLE_DIR=apps/desktop/dist cargo run --package cursor-server --bin cursor-server
endif

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
