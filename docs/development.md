# 开发与发布指南

修改与推送前核对 `git remote -v` 和 `git branch --show-current`。本仓库只服务于 haxsd byok，隔离约定见 [BRANCH_ISOLATION.md](../BRANCH_ISOLATION.md)。

## 结构与主流程

```text
apps/desktop/
├─ src/                 # React 界面、API 调用、状态与教程
└─ src-tauri/           # 桌面启动、托盘、更新、服务生命周期
server/
├─ src/local_app/       # Cursor 代理、CA、配置与退出清理
├─ src/cursor/          # Cursor 协议、会话、工具与传输
├─ src/devin/           # Devin 网关、绑定与宿主补丁
├─ src/control/         # 配置、模型与统计管理 API
├─ src/provider/        # OpenAI / Anthropic 请求适配
├─ src/run/             # 模型运行、重试、取消与工具轮次
├─ src/store/           # SQLite、内容存储与清理
├─ src/plugin/          # 插件环境与协议
└─ migrations/          # 数据库版本迁移
crates/semble-core/     # 代码索引与搜索
.github/workflows/      # CI、构建与签名发布
```

桌面启动服务 → 客户端进入 Cursor 或 Devin 模块 → 解析模型与会话 → Provider 请求 → 流式结果回到客户端 → Store 保存调用与统计。界面经本机 API 读取和修改配置。

## 构建与验证

需要 Node.js 22 或更新的兼容版本、Rust stable 及 [Tauri 平台依赖](https://v2.tauri.app/start/prerequisites/)。Windows 正式构建使用 MSVC 与 WebView2。

```bash
npm --prefix apps/desktop ci
npm --prefix apps/desktop run check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
npm --prefix apps/desktop run tauri:build -- --bundles nsis
```

本机 Windows 桌面单元测试存在加载器问题，见 [调查记录](./troubleshooting.md)。该环境可运行 `cargo test --locked --workspace --exclude haxsd-byok-desktop`，结合桌面构建、实际启动与平台 CI；这不代表桌面单元测试已经运行。Windows CI 桌面测试由 `RUN_DESKTOP_UNIT_TESTS` 控制。

Windows 检出保持 LF，避免 CRLF 改变提示词前缀。可设置 `git config core.autocrlf false` 与 `git config core.eol lf`；不要为了改行尾覆盖工作区修改。

## 发布

1. 同步 `apps/desktop/package.json`、`apps/desktop/src-tauri/Cargo.toml`、`tauri.conf.json` 的版本，同时更新两个 lock 文件的对应条目。
2. 验证并提交到本仓库 `main`。
3. 创建并推送产品标签，例如 `haxsd-byok-v1.0.18`；每次代码发布使用递增版本。
4. `release.yml` 构建 Windows 安装包、签署 Tauri 更新、生成清单并公开发布。
5. 验证流程成功、Release 非草稿、清单匿名可读、版本一致、公钥可验证签名；再验证覆盖安装、启动和配置保留。

产物包含 `x64-setup.exe`、`.sig` 与 `latest.json`。手动触发 Release desktop app 只产生 Actions 构建产物，不发布正式 Release。签名私钥不得进入源码、日志或反馈。

上游同步见 [UPSTREAM.md](../UPSTREAM.md)。两产品不互相移植；与上游历史没有共同祖先，不可直接合并上游 main。
