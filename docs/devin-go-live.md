# haxsd byok 上线操作手册

这份手册是整个项目的最后一环：把已经验证过的零件，按顺序装成"Devin 真的在用你的模型"。
它只写已经验证过的步骤；每一步都给出可核对的凭据。

## 最终产物是什么

一个能跑的 `haxsd byok` 桌面应用（内嵌服务端，数据目录在**用户主目录**下：
`%USERPROFILE%\.haxsd-byok-devin-v3`，本机即 `C:\Users\Administrator\.haxsd-byok-devin-v3`），
加上一台已经切到它的 Devin 安装。

数据流：

```text
Devin 客户端
   │  Connect 帧（本机回环）
   ▼
haxsd byok 的 Devin 网关（默认关闭，只在设置里显式启用后才监听）
   │  按 Devin 模型 UID 查绑定 → 取该绑定生效路由的模型哈希
   ▼
haxsd byok 的模型通道（你在界面里配置的模型与密钥）
   ▼
你自己的模型服务
```

## 现状（本机，2026-09-21 核对）

| 环节 | 状态 |
| --- | --- |
| 网关功能 | 已验证：设置持久化、目录、AssignModel、流式往返、工具调用、隔离性 |
| 宿主补丁引擎 | 已验证：在真实 Devin 文件的副本上跑通 打补丁 → 校验 → 还原 |
| Devin 安装 | 已被厂商路由器打过补丁，端点为 `127.0.0.1:43100/43101/43102` |
| 厂商路由器 | 未运行，43100 无监听 |
| 桌面应用 | **尚未安装**（无进程、无安装目录） |
| 数据目录 | 存在但只有空的 `rules\`，无数据库与日志，即产品从未真正运行过 |
| 安装包 | 本地旧副本已清理（2026-09-24）；需要时按第 1 步重新构建，或从 Release 下载 |

## 第 1 步：拿到并安装桌面应用

本地构建（需要 Node 22、Rust、Tauri 依赖）：

```powershell
cd D:\cursor-byok\byok-dev\haxsd-byok
npm --prefix apps/desktop ci          # 首次需要
npm --prefix apps/desktop run tauri:build -- --bundles nsis
# 产物：target\release\bundle\nsis\haxsd byok_<版本>_x64-setup.exe
```

或在 CI 上构建后下载产物（不发布 Release）：

```powershell
gh workflow run release.yml --repo haxsd/haxsd-byok --ref main
gh run watch --repo haxsd/haxsd-byok
gh run download --repo haxsd/haxsd-byok --name manual-build-windows --dir .
```

已在本机构建并校验过一份（2026-09-21，run `35578406854`；本地副本已随 2026-09-24 清理删除）：

```
haxsd byok_1.0.1_x64-setup.exe
20 462 559 字节
SHA-256 AC4FC03B461A806EA6954206C79315C113032D1B75B3ACC4BA5D1B239A2223C1
```

未签名，属于预期：本项目没有代码签名证书，安装时 Windows 会提示来源未知。

> [!IMPORTANT]
> 产物路径有过一次真实缺陷：Tauri 把安装包输出到**工作区级** `target/release/bundle/nsis/`，
> 而工作流原本去 `apps/desktop/src-tauri/target/...` 找，配合 `if-no-files-found: warn`，
> 于是"构建成功但没有产物"。现已修正路径并把缺失产物改为显式失败。

装完后确认数据目录出现（在用户主目录下，不是 `%APPDATA%`）：
`%USERPROFILE%\.haxsd-byok-devin-v3`

## 第 2 步：配置网关并绑定模型

打开应用 → 侧边栏 **Devin**（路径 `/harness/devin`，页面标题「Devin 接入」）：

1. 先在「模型」页确认至少有一个可用的模型（网关只转发，不提供模型）。
2. 打开 **启用 Devin 网关**。
3. **添加映射**：Devin 模型 UID 自己起名（例如 `MODEL_CLAUDE_4_SONNET_BYOK`），
   并选中要用的那个模型。
4. 保存。界面会提示「Devin 设置已保存，重启软件后监听端口生效」——
   **端口只在启动时读取，必须重启应用**。
5. 令牌可选：留空表示不校验；填了则 Devin 请求需带
   `x-devin-router-token` 或 `Authorization: Bearer <令牌>`。

默认端口：API/目录 `43110`、推理 `43111`、本机 API `43112`（都只绑 `127.0.0.1`）。

## 第 3 步：把 Devin 切到这个网关

**为什么要交换文件**：Devin 的端点写在它自己的 `extension.js` 里。厂商路由器已经把它改成
`43100/43101/43102`，而本项目的补丁只接受"干净的已知版本"，直接打会被 fail-closed 拒绝：

```
Devin host file is already patched (ports 43100 / 43101 / 43102);
restore the clean version first, for example the router's own backup next to the file,
and then apply this patch
```

所以顺序是"先回到干净版本，再打我们的补丁"。厂商把干净版本留在了原文件旁边：

```
D:\devin\Devin\resources\app\extensions\windsurf\dist\
  extension.js                               ← 现用（厂商已打补丁）
  extension.js.devin-model-router.backup     ← 干净版本（9 739 343 字节）
```

**先在副本上练一遍**（强烈建议，避免误伤安装）：

```powershell
$dist = "D:\devin\Devin\resources\app\extensions\windsurf\dist"
$work = "$env:TEMP\devin-switch"
New-Item -ItemType Directory -Force -Path $work | Out-Null
Copy-Item "$dist\extension.js.devin-model-router.backup" "$work\extension.clean.js"
```

在 Devin 页面底部的宿主补丁区：
1. 路径填 `$work\extension.clean.js`，点状态检查 → 应显示 `clean`、可兼容；
2. 点「应用补丁」→ 端口应显示 `43110 / 43111 / 43112`；
3. 点「恢复原文件」→ 文件应回到与厂商备份**逐字节相同**。

副本上这三步都符合预期后，再对真实文件执行同样的三步，然后**重启 Devin**。

## 第 4 步：验证真的通了

在 Devin 里发起一次对话，然后看 haxsd byok 的「调用记录」页。成功的标志：

- 出现一条 `call_id` 形如 `devin:<executionId>` 的记录；
- `provider_type` 是你配的协议（如 `openai-chat`），`status` 为 `completed`；
- 有 token 计数。

命令行等价核对（把端口换成你服务端的服务端口）：

```powershell
Invoke-RestMethod "http://127.0.0.1:<服务端口>/__byok-api__/api/llm-calls" |
  Where-Object call_id -like "devin:*" | Select-Object call_id,status,finish_reason
```

## 回退

| 想退回到 | 操作 |
| --- | --- |
| 厂商路由器 | 在 Devin 页面点「恢复原文件」（用打补丁时生成的 receipt），重启 Devin，再启动厂商路由器 |
| 完全不接 Devin | 在 Devin 页面关闭「启用 Devin 网关」并重启应用；Devin 侧保持原样 |

补丁每次都会在目标文件旁写一份带 SHA-256 的备份（`*.devin-router.backup`）；
文件被外部改过或备份对不上时，恢复会拒绝执行。
