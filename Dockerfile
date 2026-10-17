# syntax=docker/dockerfile:1.7

FROM node:22-bookworm-slim AS web
WORKDIR /src/apps/desktop
COPY apps/desktop/package.json apps/desktop/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci
COPY apps/desktop/index.html apps/desktop/tsconfig.json apps/desktop/tsconfig.node.json apps/desktop/vite.config.ts ./
COPY apps/desktop/plugins/ plugins/
COPY apps/desktop/src/ src/
RUN npm run build

FROM rust:1-bookworm AS server
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY server/ server/
# 工作区成员必须一起复制:cargo 解析 Cargo.toml 时会读取每个成员的清单,
# 而 cursor-server 依赖 crates/semble-core(server/Cargo.toml 的 path 依赖)。
COPY crates/ crates/
COPY support/benchmarks/semble/ support/benchmarks/semble/
COPY apps/desktop/src-tauri/Cargo.toml apps/desktop/src-tauri/Cargo.toml
COPY apps/desktop/src-tauri/build.rs apps/desktop/src-tauri/build.rs
COPY apps/desktop/src-tauri/src/ apps/desktop/src-tauri/src/
COPY apps/desktop/src-tauri/capabilities/ apps/desktop/src-tauri/capabilities/
COPY apps/desktop/src-tauri/icons/ apps/desktop/src-tauri/icons/
COPY apps/desktop/src-tauri/tauri.conf.json apps/desktop/src-tauri/tauri.conf.json
COPY protocols/cursor/ protocols/cursor/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --package cursor-server --bin cursor-server && \
    cp target/release/cursor-server /tmp/cursor-server

FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install --yes --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin haxsd-byok && \
    mkdir -p /app/console /data && \
    chown haxsd-byok:haxsd-byok /data

COPY --from=server /tmp/cursor-server /usr/local/bin/cursor-server
COPY --from=web /src/apps/desktop/dist/ /app/console/

# 容器里必须绑 0.0.0.0 才能被端口映射访问，而网关默认拒绝非回环监听（Cursor 侧路由表
# 本身没有独立的鉴权层，直接绑到网卡上等于把同网段的主机都放进来）。容器是例外，但要显式
# 写出来：容器自身的网络命名空间就是信任边界——谁能连到它由编排层的端口映射/反代/网络
# 策略决定，而不是由监听地址决定。不设这个开关，镜像一启动就会带着拒绝信息退出。
ENV CURSOR_LISTEN_ADDR=0.0.0.0:3000 \
    HAXSD_BYOK_ALLOW_REMOTE_LISTEN=1 \
    HAXSD_BYOK_DATABASE_URL=sqlite:///data/haxsd-byok.db \
    CURSOR_CONSOLE_DIR=/app/console \
    RUST_LOG=cursor_server=info

USER haxsd-byok
EXPOSE 3000
VOLUME ["/data"]
HEALTHCHECK --interval=10s --timeout=3s --retries=5 \
    CMD curl --fail --silent http://127.0.0.1:3000/__byok-api__/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/cursor-server"]
