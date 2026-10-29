<div align="center">

# haxsd byok

**在 Cursor、Devin、ChatGPT 客户端里的 Codex，以及 Claude Code CLI 与 Claude Desktop 中使用你自己配置的模型：本地模型网关，自带模型库、接管状态和调用记录。**

[![Release](https://img.shields.io/github/v/release/haxsd/haxsd-byok)](https://github.com/haxsd/haxsd-byok/releases/latest)
[![Platform](https://img.shields.io/badge/platform-Windows%20x64-blue)](https://github.com/haxsd/haxsd-byok/releases/latest)
[![License](https://img.shields.io/badge/license-MIT-blue)](./LICENSE)

[下载 Windows 安装包](https://github.com/haxsd/haxsd-byok/releases/latest) · [用户指南](./docs/user-guide.md) · [故障排查](./docs/troubleshooting.md) · 简体中文 · [English](./README-EN.md) · [Русский](./README-RU.md)

</div>

![haxsd byok 主界面](./images/en-home-1.png)

## 为什么用它

- **不换工作流**：继续用 Cursor、Devin、Codex、Claude Code CLI 与 Claude Desktop 的界面和习惯，只把模型推理换成你配置的服务。
- **模型自己管**：地址、API Key、模型 ID 都存在本机；Cursor 还支持官方 / 本地 / 混合三种模型来源，官方模型和自配模型可以同时可用。
- **成本看得见**：每次调用的成败、耗时、Token 用量，以及按参考单价估算的金额，都在本机记录。

模型服务需要自备。Key 与设置保存在本机，请求发往你配置的服务商。

## 能做什么

| 能力 | 说明 |
|---|---|
| 模型库 | 地址、API Key、模型 ID；支持 OpenAI Chat / Responses 与 Anthropic 协议；发现后多选添加、带连接测试 |
| Cursor 接入 | 官方 / 本地 / 混合三种模型来源，含本机代理、本地 CA 与接管状态 |
| Cursor 上下文压缩 | 本机模型长会话的自动与手动摘要；保留工程交接、历史/当前状态、压缩归档与摘要详细度重试 |
| Devin 接入 | 可选本机网关、显式默认模型与模型映射、Devin 3.11.216 版本化兼容补丁；支持自定义安装目录 |
| Codex 接入 | ChatGPT 客户端里的本地 Codex 保留工具、终端、审批、沙箱与任务循环，只替换模型推理；支持 Responses / Chat Completions / Anthropic |
| Claude 接入 | 本机 Anthropic 网关（`/v1/messages`），适配 Claude Code CLI 与 Claude Desktop；Desktop 受组织策略托管时只给手工配置引导 |
| 调用观测 | 调用成败、耗时、Token 用量，按参考单价估算金额 |
| 桌面管理 | 中英俄三语、多主题、托盘、应用内教程与签名更新 |

### 能力边界

| 客户端 | 本机接管可用 | 仍需官方账号或云端 |
| --- | --- | --- |
| Cursor | Agent / Chat / Plan、本机子代理与工具、Rules / Skills / MCP、Checkpoint | Cloud Agents、Projects、Bugbot、Automations、Tab 补全、CLI / IDE 插件、Remote Control |
| Codex | 本地 Agent / TUI / exec、Plan、工具、审批、沙箱、多会话、MCP / Skills / Plugins / Hooks、图片输入 | ChatGPT Work、Codex Cloud、Remote Control、官方托管搜索 / 图像 / 语音 |
| Devin | Cascade、Devin Local、本机工具、Rules / Memories / MCP、模型映射、Checkpoint | 账号套餐与额度、Tab / Command / embedding、Devin Cloud、远程索引、Slack / Linear / Jira / Teams / GitHub 集成 |
| Claude | Claude Code CLI、本机子代理与后台会话、MCP / Skills / Hooks、Checkpoint / Rewind / 自动压缩、本机 IDE 集成 | Cloud session、Teleport、Ultrareview、云端定时任务、Remote Control、供应商侧 WebSearch / WebFetch、CLI 到 Desktop 的 handoff |

### Cursor 接管后的上下文压缩

Cursor 原本使用它自己的“上下文窗口 + 自动摘要”机制。开启本地或混合模式后，请求由本机网关接管，Cursor 官方的摘要流程不再位于请求路径；因此这部分改由本应用的网关执行。官方模式不经过本应用，仍由 Cursor 自己压缩。

接管只改变本机可控制的模型请求；登录、套餐、额度和厂商云端功能仍由对应客户端管理，不会绕过。

## 支持矩阵

| 客户端 | 接管方式 | 注意 |
|---|---|---|
| Cursor | 官方 / 本地 / 混合模型来源 + 本地 CA | **开启本地或混合模式会结束正在运行的 Cursor，先保存工作**；只想让自配模型生效选本地，想让官方模型同时可用选混合 |
| Devin | 可选本机网关 + 模型映射 | 默认关闭，只监听 `127.0.0.1`；改动端口后需重启应用，退出 Devin 后重新写入接入配置 |
| Codex（ChatGPT 客户端） | 本机网关 + “应用到 Codex” | 保留工具、终端、审批、沙箱与任务循环，只替换推理；默认关闭，只监听 `127.0.0.1` |
| Claude Code CLI | 本机 Anthropic 网关 + 写入 `~/.claude/settings.json` 的 `env` | 默认关闭，只监听 `127.0.0.1`；应用后重新打开 Claude Code；已有 Anthropic API Key、Bedrock/Vertex/Foundry 或 `apiKeyHelper` 时拒绝覆盖；存在 `managed-settings.json` 或注册表策略时状态为「受管配置」，不改写 |
| Claude Desktop | 本机 Anthropic 网关 + 写入 `%LOCALAPPDATA%\Claude-3p\configLibrary` 的 profile | **应用前完全退出 Claude Desktop（含托盘），应用后重新启动**；组织策略（HKLM/HKCU）托管时只读并给手工引导，本应用不写策略 |

## 快速开始

**系统要求**：Windows x64；一个可用的模型服务（OpenAI Chat Completions / Responses 或 Anthropic 兼容接口）。

1. 从 [Releases](https://github.com/haxsd/haxsd-byok/releases/latest) 下载名称含 `x64-setup.exe` 的安装包并安装。
2. 在「模型」页添加地址、API Key 与模型 ID，选择协议，用「测试」确认可用。
3. **用 Cursor**：初始化本地 CA 并按提示信任，再到「Cursor」页选择模型来源。开启本地或混合模式会结束正在运行的 Cursor，先保存工作。
4. **用 Devin**：选择默认模型、设置模型映射、启用网关并保存；如果 Devin 不在默认位置，在「高级」里填写安装目录。重启应用后按 [Devin 接入](./docs/devin-go-live.md) 写入接入配置并验证。
5. **用 Codex**：在「Codex」页选择模型、启用网关并保存，点「应用到 Codex」，然后重新打开客户端或新建会话；详见 [Codex 接入](./docs/codex-go-live.md)。
6. **用 Claude**：在「Claude」页选择默认模型、启用网关并保存，然后按客户端应用接管：Claude Code CLI 应用后重新打开 Claude Code；Claude Desktop 需先完全退出（含托盘），应用接管后再启动。详见 [用户指南](./docs/user-guide.md)。

地址填写、升级、备份与恢复的完整步骤见 [用户指南](./docs/user-guide.md)。

![模型库与连接测试](./images/en-model-1.png)

## 数据、隐私与安全

- Key、设置与调用记录保存在本机数据库：`%USERPROFILE%\.haxsd-byok-devin-v3`。
- 模型请求发往你配置的服务商；Key、设置与调用记录保存在本机。
- 本地 CA 私钥只在本机，用于 Cursor 本地代理；不要把它或整个数据库发给别人。
- 备份请按敏感文件保管，不要上传 API Key、CA 私钥或完整数据库到 Issue。

## 使用边界与合规

- 本地与混合模式会改写 Cursor 的代理配置，切换模式需要重启 Cursor；官方模式不写配置，也不产生调用记录。
- 本地模式只认识模型库里的模型，不在库里的会被明确拒绝，不会静默走官方模型。
- Devin 与 Codex 接入默认关闭，只监听 `127.0.0.1`；Codex 接入只改写该客户端 `config.toml` 的受管字段，撤销时恢复，不覆盖登录、MCP、技能或审批设置。
- Claude 订阅账号接入（[说明](./docs/claude-account.md)）用订阅 OAuth 作为模型来源，可能违反 Anthropic 的条款并导致限流或封号；优先使用 Anthropic API Key。本应用不伪装客户端，也无法保证平台不处罚。
- 本产品与 [haxsd/cursor-byok](https://github.com/haxsd/cursor-byok) 使用不同的数据目录、标识与更新渠道，两者互不影响。
- 本项目与 Cursor、Devin、OpenAI、Anthropic 等厂商无隶属关系。接入会改写客户端本地配置，请自行确认并遵守目标客户端与模型服务商的条款。

## 常见问题

- **Windows 提示“未知发布者”或 SmartScreen 拦截？** 更新包使用本产品自己的 Tauri 更新签名，不等同于 Windows 代码签名。选择「更多信息 → 仍要运行」即可；正式分发版本以 Releases 页为准。
- **支持哪些模型？** 任何兼容 OpenAI Chat Completions / Responses 或 Anthropic 协议的服务，地址、Key、模型 ID 都由你填。
- **会上传我的 Key 或调用记录吗？** Key、设置和调用记录保存在本机；请求只发往你配置的模型服务商。
- **怎么回滚？** 切回官方模式后 Cursor 不再经过本地网关；Codex 侧可撤销“应用到 Codex”，恢复原配置；Claude 侧在对应标签页撤销接管，移除写入 `settings.json` 的键或 Desktop 的 profile。详细步骤见 [用户指南](./docs/user-guide.md)。
- **和 cursor-byok 是什么关系？** 同一作者的两个独立产品，数据目录、标识和更新渠道都不同；本项目在其基础上重做，见 [UPSTREAM.md](./UPSTREAM.md)。

## 从源码构建与参与

开发环境、构建与验证步骤见 [开发说明](./docs/development.md)；仓库根目录的 `Makefile` 汇总了常用命令。第三方组件与许可见 [THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md)。

## 反馈与许可

遇到问题先看 [故障排查](./docs/troubleshooting.md)，再提 [Issue](https://github.com/haxsd/haxsd-byok/issues)。请提供版本与出错步骤，不要上传 API Key、CA 私钥或整个数据库。

基于 [leookun/cursor-byok](https://github.com/leookun/cursor-byok)，沿用 [MIT 许可证](./LICENSE)。
