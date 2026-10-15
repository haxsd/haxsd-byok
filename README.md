<div align="center">

# haxsd byok

让 Cursor、Devin 和 ChatGPT 客户端中的 Codex 使用你配置的模型 API：本机模型网关，含模型库、接入状态与调用记录。

[下载 Windows 安装包](https://github.com/haxsd/haxsd-byok/releases/latest) · [用户指南](./docs/user-guide.md) · [Devin 接入](./docs/devin-go-live.md) · [Codex 接入](./docs/codex-go-live.md) · [故障排查](./docs/troubleshooting.md) · [English](./README-EN.md)

</div>

## 能做什么

- **模型库**：地址、API Key、模型 ID；OpenAI Chat / Responses 与 Anthropic；带连接测试。
- **Cursor 接入**：官方 / 本地 / 混合三种模型来源，含本机代理、本地 CA 与接管状态。
- **Devin 接入**：可选本机网关，模型绑定与宿主补丁。
- **Codex 接入**：ChatGPT 客户端里的本地 Codex 保留工具、终端、审批、沙箱与任务循环，只把模型推理换成你的模型；支持 Responses / Chat Completions / Anthropic。
- **调用观测与价值估算**：调用成败、耗时、Token 用量，按参考单价估算金额。
- **桌面管理**：中英双语、多主题、托盘、应用内教程与签名更新。

模型服务需要自备。Key 与设置保存在本机，请求发往你配置的服务商。

## 快速开始

1. 从 [Releases](https://github.com/haxsd/haxsd-byok/releases/latest) 下载名称含 `x64-setup.exe` 的安装包并安装。
2. 在「模型」页添加地址、API Key 与模型 ID，选择协议，用「测试」确认可用。
3. 用 Cursor：初始化本地 CA 并按提示信任，再到「Cursor」页选择模型来源。**开启本地或混合模式会结束正在运行的 Cursor，先保存工作**；想让官方模型与你的模型同时可用就选「混合」。
4. 用 Devin：设置模型映射、启用网关并保存，重启应用后按 [Devin 接入](./docs/devin-go-live.md) 完成宿主侧。
5. 用 Codex：在「Codex」页选择模型、启用网关并保存，再点「应用到 Codex」，然后重新打开客户端或新建会话；详见 [Codex 接入](./docs/codex-go-live.md)。

地址填写、升级、备份与恢复的完整步骤见 [用户指南](./docs/user-guide.md)。

## 下载与升级

- 目前提供 **Windows x64 安装包**（NSIS）；应用内可在「系统设置 → 软件更新」检查并安装。
- 更新包来自本仓库，用本产品独立的 Tauri 更新签名校验；它不等同于 Windows 代码签名，SmartScreen 可能提示来源，选择「更多信息 → 仍要运行」即可。
- 数据目录为 `%USERPROFILE%\.haxsd-byok-devin-v3`，含数据库、证书与运行数据。API Key 存在本地数据库里，备份请按敏感文件保管。

## 使用边界

- 本地与混合模式会改写 Cursor 的代理配置，切换模式需要重启 Cursor；官方模式不写配置，也不产生调用记录。
- 本地模式只认识模型库里的模型，不在库里的会被明确拒绝，不会静默走官方。
- Devin 默认关闭，只监听 `127.0.0.1`；改动端口后需重启应用并重新应用宿主补丁。
- Codex 接入默认关闭，只监听 `127.0.0.1`；它只在点「应用到 Codex」时改写该客户端的 `config.toml` 受管字段，撤销时恢复。它不覆盖登录、MCP、技能或审批设置。
- 本产品与 [haxsd/cursor-byok](https://github.com/haxsd/cursor-byok) 使用不同的数据目录、标识与更新渠道。

## 反馈与许可

遇到问题先看 [故障排查](./docs/troubleshooting.md)，再提 [Issue](https://github.com/haxsd/haxsd-byok/issues)。请提供版本与出错步骤，不要上传 API Key、CA 私钥或整个数据库。

基于 [leookun/cursor-byok](https://github.com/leookun/cursor-byok)，沿用 [MIT 许可证](./LICENSE)。
