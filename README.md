<div align="center">

# haxsd byok

让 Cursor 和 Devin 使用你配置的模型 API，在本机管理模型、接入状态与调用记录。

[下载 Windows 安装包](https://github.com/haxsd/haxsd-byok/releases/latest) · [上手与升级](./docs/user-guide.md) · [Devin 接入](./docs/devin-go-live.md) · [故障排查](./docs/troubleshooting.md) · [English](./README-EN.md)

</div>

## 能做什么

| 功能 | 说明 |
| --- | --- |
| 模型库 | 配置地址、API Key 和模型 ID；支持 OpenAI Chat / Responses、Anthropic，新增、复制、排序与连接测试 |
| Cursor 接管 | 本机代理与本地 CA 接入；可查看证书、配置归属和接管状态 |
| Devin 接入 | 可选本机网关，模型 UID 绑定、手动候选路由，以及有校验和备份的宿主补丁 |
| 调用观测 | 查看调用成败、耗时、Token 用量；按设置记录请求与响应详情 |
| 价值估算 | 可编辑的固定或高峰/低谷单价；人民币与美元价格分别保存 |
| 桌面管理 | 简体中文 / English、多主题、托盘、应用内教程与签名更新 |

```text
Cursor → 本机代理与 CA ─┐
                        ├→ 模型库 → 你配置的模型 API
Devin  → 可选本机网关 ─┘        └→ 本地调用记录与统计
```

模型服务需要自行提供。Key 和设置保存在本机，请求会发送到你配置的服务商。价值估算使用应用内参考单价，不等同于服务商账单。

## 五分钟上手

1. 从 [Releases](https://github.com/haxsd/haxsd-byok/releases/latest) 下载名称含 `x64-setup.exe` 的 Windows 安装包并安装。
2. 打开「模型」，添加地址、API Key 和模型 ID，选择正确协议。「测试」会发送一次真实模型请求。
3. 使用 Cursor：初始化本地 CA，按提示完成信任，再开启接管。**先保存工作：开启接管会结束正在运行的 Cursor 进程，需要手动重新打开。**
4. 使用 Devin：设置模型映射、启用网关并保存，重启 haxsd byok，再按 [Devin 手册](./docs/devin-go-live.md) 接入宿主并重启 Devin。
5. 在客户端发起对话，到「调用」页确认记录、模型和结果。只使用一个客户端时，另一个模块无需配置。

详细地址填写、升级、备份和恢复步骤见 [用户指南](./docs/user-guide.md)。

## 下载与升级

正式发布目前提供 **Windows x64 NSIS 安装包**。其他平台有源码与 CI 检查，Releases 尚未提供 macOS / Linux 安装包。

- 应用内：在「系统设置 → 软件更新」检查并安装。
- 手动升级：退出应用，再运行新版安装包。安装器复用原有安装位置，数据保存在独立目录。
- 更新来自本仓库，使用本产品独立的 Tauri 更新签名。
- 更新签名用于校验更新包，不等同于 Windows Authenticode 代码签名；Windows 仍可能显示来源提示。

**1.0.20 修复**：更新失败不再只给一句英文系统错误——会分清「被系统策略拦下（Smart App Control／代码完整性）」「权限不足」「网络/代理」并给出下一步，安装后重启会自动核对是否真的装上了；界面显示当前可执行文件与数据目录，用来分辨本机是否存在第二份安装。同时新增开发者模式（默认关）与诊断卡片（打开日志目录、复制诊断摘要），更新检查在启动时静默进行并在顶栏提示，接口刷新改为分项（单个接口失败不再让整页停在旧数据）。服务端修掉：退出后遗留的「进行中」记录、助手进程判定的 PID 复用、`http.noProxy` 被删除不归还，以及管理接口不再明文回传 API Key。[发布说明](https://github.com/haxsd/haxsd-byok/releases/tag/haxsd-byok-v1.0.20)

## 使用边界与数据

- 接管期间保持应用运行。停用时，Cursor 先关闭接管；Devin 先恢复宿主文件，再关闭网关。
- Devin 默认关闭，仅监听 `127.0.0.1`，默认端口为 `43110/43111/43112`。修改端口后需重启应用并重新应用宿主补丁。
- Devin 候选路由由用户手动切换，失败时不会自动换路由。宿主补丁只接受可识别版本，客户端升级后需重新检查。
- 默认数据目录为 `%USERPROFILE%\.haxsd-byok-devin-v3`，含数据库、证书与运行数据。API Key 保存在本地数据库中，备份也应按敏感文件保管。
- 请求与响应详情、闲置会话运行状态有保留期限，累计用量统计保留。[数据保留规则](./docs/user-guide.md#记录与数据保留)
- 本产品与 [haxsd/cursor-byok](https://github.com/haxsd/cursor-byok) 使用不同的数据、标识与更新渠道。[仓库隔离规则](./BRANCH_ISOLATION.md)

## 开发与反馈

结构、构建、验证范围和发布步骤见 [开发指南](./docs/development.md)；协议与验证证据见 [Devin 集成说明](./docs/devin-integration.md)。[1.0.18 质量检查报告](./docs/quality-review-2026-09-28.md)记录了本轮确认的问题与建议。

遇到问题先看 [故障排查](./docs/troubleshooting.md)，再提交 [Issue](https://github.com/haxsd/haxsd-byok/issues)。提供版本、出错步骤和脱敏错误信息，不要上传 API Key、CA 私钥或整个数据库。

## 致谢与许可

基于 [leookun/cursor-byok](https://github.com/leookun/cursor-byok)，沿用 [MIT 许可证](./LICENSE)。感谢上游作者与贡献者。本产品加入独立发布与更新、Devin 接入、分时估算和界面调整，并移除广告入口。上游文档可作背景参考，使用本产品请以这里的指南与应用内教程为准。
