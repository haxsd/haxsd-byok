# Claude 订阅账号接入（OAuth）

这个插件用 Claude Pro/Max 订阅的 OAuth 授权作为模型来源，把请求转发到 Anthropic 的 Messages API。
它不是 Anthropic 官方支持的接入方式。**使用前请先读风险一节。**

## 风险

- **可能违反服务条款。** Anthropic 的消费者条款要求订阅凭证只配合官方客户端使用。用订阅 OAuth 驱动第三方客户端，可能被认定为违规。
- **可能导致限流或封号。** Anthropic 可以按客户端特征、请求头、模型和用量限制或暂停账号。风险由使用者承担。
- **上游可能直接拒绝。** 社区实现 `hermes-claude-auth` 的 README 称，Anthropic 在 2026-04-04 增加了服务端校验，第三方 OAuth 请求会被拒或改为消耗 extra usage（该说法未获 Anthropic 官方确认）。社区代理通常靠伪造 Claude Code 的 User-Agent、计费头、系统提示和工具命名来绕过；**本插件不做任何伪造**，因此可能登录成功但调用失败。遇到 `third-party` / `extra usage` 的 400，说明上游不接受这种用法，请改用 API Key。
- **凭证明文保存在本机。** access token、refresh token 存在本应用的 SQLite 资源记录里，没有额外加密。能读取本机数据目录或数据库的人可以拿走凭证。
- **上游变化会失效。** OAuth 客户端 ID、授权/令牌端点、scope、回调地址、模型 ID 和请求头都可能随时改变，插件不保证长期可用。
- **建议优先使用 Anthropic API Key。** API Key 是官方支持的路径；订阅接入只适合明确接受上述风险、并愿意自行承担后果的用户。

## 我们如何降低风险

插件把自己限制在“一个行为良好的 API 客户端”范围内，不做任何规避识别的伪装。具体行为：

- **不伪装客户端。** 不冒充 `claude-cli/*` User-Agent，不伪造 `x-anthropic-billing-header` 或设备指纹，不注入自定义 `x-*` 请求头；只发送 `anthropic-version` 和 OAuth 必需的 `oauth-2025-04-20`，请求体不带 `metadata.user_id`，也不声明 1M 上下文、`context_management` 等当前请求用不到的 beta 能力。
- **不并发轰炸。** 每个账号最多 2 个并发推理请求，超出的在进程内排队；429 后该账号进入冷却，冷却期间不再发新请求。
- **尊重上游的节奏。** 429 优先严格按 `retry-after` 冷却；没有该头才使用带抖动的指数退避。5xx 最多重试 2 次，其它 4xx 不重试。401 只刷新一次、只重试一次，仍失败就标记为需要重新登录，不循环刷新。
- **提前刷新，且只刷新一次。** access token 在到期前 4 小时刷新；同一账号的刷新做 single-flight，避免并发刷新把单次 refresh token 用两次。只对明确的 429/5xx 重试刷新；传输层错误不自动重试，因为请求可能已被上游消费。
- **令牌不出现在日志和界面里。** 错误消息、资源状态和资源面板都会抹掉 access/refresh token；刷新失败保留旧凭据，不用空值覆盖。
- **失败即停。** 刷新失败、授权被拒或上游返回不可重试的错误时，账号进入 `invalid`/`cooling`，插件不会换账号重试，也不会用轮换账号的方式绕开限额。

这些措施能减少“因为我们的行为不当而被限流或处罚”的机会，但**无法保证平台不处罚**：订阅 OAuth 用于第三方客户端本身就可能违反 Anthropic 的服务条款，上游仍可仅凭客户端身份拒绝或封禁。需要稳定、可预期的接入时，请优先使用 API Key。

## 已核实事实（2026-10-23）

“已核实”指至少两个公开实现或官方资料一致；没有官方文档背书的项目标为“社区核实”。

| 项目 | 值 | 核实方式 |
| --- | --- | --- |
| OAuth 客户端 ID | `9d1c250a-e61b-44d9-88ed-5944d1962f5e` | CLIProxyAPI、opencode-claude-auth、opencode-anthropic-oauth 一致（社区核实） |
| 授权端点 | `https://claude.ai/oauth/authorize` | 同上 |
| 令牌交换/刷新端点 | `https://platform.claude.com/v1/oauth/token` | CLIProxyAPI 注释明确写明 Claude Code 2.1.220 使用该域名；opencode-anthropic-oauth 也使用它（社区核实） |
| scope | `user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload` | 三个公开实现一致（社区核实） |
| 回调地址 | `http://localhost:54545/callback` | CLIProxyAPI 硬编码；旧版 Claude Code 也使用 54545（社区核实） |
| 授权参数 | `code=true`、`response_type=code`、`client_id`、`redirect_uri`、`scope`、`code_challenge`、`code_challenge_method=S256`、`state` | CLIProxyAPI `GenerateAuthURL` 与 opencode-anthropic-oauth 一致 |
| 授权码交换 | `POST` JSON：`grant_type=authorization_code`、`code`、`redirect_uri`、`client_id`、`code_verifier`、`state` | CLIProxyAPI `ExchangeCodeForTokens`；opencode-anthropic-oauth 使用 `application/x-www-form-urlencoded`，说明端点两种编码可能都接受，本插件采用 CLIProxyAPI 观察到的 JSON |
| 刷新令牌 | `POST` JSON：`client_id`、`grant_type=refresh_token`、`refresh_token`、`scope` | CLIProxyAPI `refreshTokensSingleFlight` |
| Messages API 鉴权 | `Authorization: Bearer <access_token>`，并带 `anthropic-version: 2023-06-01`、`anthropic-beta: oauth-2025-04-20` | opencode-claude-auth 的请求头；opencode-anthropic-oauth 的 beta 列表包含 `oauth-2025-04-20`（社区核实） |
| 账号资料 | `GET https://api.anthropic.com/api/oauth/profile`，Bearer 认证，返回 `account.uuid`、`account.email`、`organization.uuid`、`organization.name` | CLIProxyAPI `ProfileURL` 与 `OAuthProfile`；本机无凭证请求该端点返回 401“Missing Authorization header. Please provide an OAuth token as a Bearer token.”，说明端点存在且接受 Bearer |
| 订阅用量 | `GET https://api.anthropic.com/api/oauth/usage` 存在；本机无凭证请求返回 401“x-api-key header is required” | 只验证了路径存在，**没有验证订阅 OAuth 能否读取额度**，因此不做额度面板 |

### 模型与上下文

当前官方阵容（Fable 5.1 / Opus 5.5 / Sonnet 5.5 / Haiku 5.5）来自
[Anthropic Models overview](https://platform.claude.com/docs/en/about-claude/models/overview)。模型 ID、上下文窗口和输出上限与
[LiteLLM 的模型价格表](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json)
交叉核对：

| 模型 | 输入上下文 | 最大输出 |
| --- | --- | --- |
| `claude-fable-5-1` | 1,000,000 | 128,000 |
| `claude-opus-5-5` | 1,000,000 | 128,000 |
| `claude-sonnet-5-5` | 1,000,000 | 128,000 |
| `claude-haiku-5-5` | 1,000,000 | 128,000 |
| `claude-opus-5` / `claude-sonnet-5` / `claude-fable-5` | 1,000,000 | 128,000 |
| `claude-opus-4-8` / `4-7` / `4-6` / `claude-sonnet-4-6` | 1,000,000 | 128,000 |
| `claude-opus-4-5` / `claude-sonnet-4-5` / `claude-haiku-4-5` | 200,000 | 64,000 |

带日期后缀的快照（如 `claude-sonnet-4-5-20250929`）也在模型目录里。SDK 的模型字段没有上下文窗口，所以插件只声明 `maxOutputTokens`；上下文窗口只记录在本表和 `models.ts` 注释里。

## 回调地址

上游登记的固定回调是 `http://localhost:54545/callback`。插件清单的 `callback` 现在可以声明
`host`（只允许 `127.0.0.1` 或 `localhost`，默认 `127.0.0.1`）；本插件声明 `localhost`，
宿主据此生成逐字一致的 `redirect_uri`，但监听 socket 仍然只绑定在 IPv4 回环地址上。

授权码既支持标准的 `?code=...&state=...`，也兼容 `code#state` 形式：宿主会从片段里取出
state 参与校验，再把去掉片段的 code 交给插件。这样即使上游把 state 附在授权码后面，
也不会因为“state 不匹配”直接登录失败。

## 插件结构

```text
server/plugins/build-in/claude-code-auth/
├── plugin.json        # apiVersion、id、网络白名单（api.anthropic.com / claude.ai / platform.claude.com）
├── deno.json          # cursor-byok:* import map
├── main.ts            # defineProviderPlugin：1 个 provider + 1 个 resource
├── oauth.ts           # authorization-code + PKCE、令牌交换、profile 读取
├── provider.ts        # Messages API 请求构造、SSE 解析、重试/冷却编排
├── runtime.ts         # 进程内并发闸门、429 冷却、刷新 single-flight
├── models.ts          # 静态模型目录（不包含上下文窗口字段）
├── resources.ts       # 账号数据、refresh_token 刷新、展示、共享常量
├── redact.ts          # 错误消息去令牌与截断
├── assets/claude.svg  # 插件图标
├── oauth_test.ts      # PKCE/授权 URL/令牌交换/过期会话
├── provider_test.ts   # SSE（thinking/text/tool/usage）、401 刷新重试、5xx/429、4 小时预刷新、single-flight
└── runtime_test.ts    # 并发上限、冷却、retry-after、single-flight
```

## 凭据与数据

- 资源类型：`claude-code-account`。
- `privateData` 字段：`accessToken`、`refreshToken`、`displayName`、`accountUuid`、`organizationName`、`expiresAtMs`、`lastRateLimitAtMs`。
- 不保存 scope、API Key 或对话内容。
- 不提供“从 Claude Code 本地凭证文件导入”。该文件的字段没有公开稳定契约，宁可没有导入，也不做按猜测解析的假导入。
- 插件不发送遥测；网络请求只发往 `plugin.json` 白名单中的三个主机。

## 未验证项

- 订阅 OAuth 令牌能否通过 Anthropic 的第三方客户端校验；社区报告倾向于“不能，除非伪造客户端特征”。
- `/api/oauth/usage` 是否接受订阅 OAuth Bearer（无凭证请求只提示需要 `x-api-key`）。
- 各订阅档位实际可用的模型；插件列出目录，不代表账号一定有权限。
- 所有模型的 adaptive thinking 与 `output_config.effort` 在 OAuth 下是否全部可用；插件对所有模型使用 adaptive（带 `display: summarized`），并在末尾工具续轮缺少签名 thinking 时自动关闭 thinking，避免上游 400。
- `claude-haiku-5-5` 等新模型对 thinking 配置的具体限制。

## 故障排查

| 现象 | 处理 |
| --- | --- |
| 浏览器显示 `redirect_uri` 不匹配 | 确认插件清单的 callback.host 是 `localhost`（本插件默认如此）；若上游更改了登记地址，需要同步修改插件后重新登录 |
| 授权开始时报端口被占用 | 54545 已被其他进程占用（例如另一个 Claude Code 登录窗口）；关闭后重试 |
| 401 / 403 | 在账号管理里点“刷新”重新取令牌；仍失败则删除账号重新登录 |
| 400 且包含 `third-party` 或 `extra usage` | 上游判定为第三方使用，插件不会伪造客户端特征；改用 API Key |
| 429 / 529 | 插件把账号标为冷却并按 `retry-after` 重试；等冷却结束或换账号 |
| 登录成功但没有模型 | 在插件设置里点“同步模型”；仍为空说明模型目录拉取失败 |

## 参考

- [CLIProxyAPI · internal/auth/claude/anthropic_auth.go](https://github.com/router-for-me/CLIProxyAPI/blob/main/internal/auth/claude/anthropic_auth.go)
- [opencode-claude-auth](https://github.com/griffinmartin/opencode-claude-auth)
- [opencode-anthropic-oauth · src/oauth.ts](https://github.com/shahidshabbir-se/opencode-anthropic-oauth/blob/master/src/oauth.ts)
- [hermes-claude-auth](https://github.com/kristianvast/hermes-claude-auth)
- [Anthropic Models overview](https://platform.claude.com/docs/en/about-claude/models/overview)
- [Anthropic TypeScript SDK · Messages 类型](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/src/resources/messages/messages.ts)
- [LiteLLM · model_prices_and_context_window.json](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json)
