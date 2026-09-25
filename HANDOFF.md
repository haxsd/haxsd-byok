# haxsd byok — 项目交接说明

> 写给接手的 agent。读完这份文档你就能独立推进，不需要重新摸索。
> 所有路径为绝对路径，所有结论都有出处（代码 / 命令输出 / 实测）。

---

## 一、这个项目是什么

**把 Cursor 和 Devin 这两个 AI 编辑器从厂商模型上「接出来」，让它们用用户自己配置的模型。**

两个客户端都只会连厂商服务器、只认厂商模型。本项目的做法是**在本机拦下它们与厂商之间的 API 通信**：

```
Cursor ──┐                                    ┌── 你的模型提供方
         ├─→ 本地网关（本项目）───────────────┤   (DeepSeek / OpenAI / …)
Devin  ──┘   模型请求本地处理                  └── 厂商服务器（登录等原样转发）
             其余方法转发
```

关键点：**登录、账号、遥测这些本地不处理的方法原样转发回厂商**，所以厂商登录仍然正常，只是模型换了。

### 最终产物

Windows 桌面应用（Tauri 2 + Rust 后端 + React 前端），交付物是 **NSIS 安装包 + 应用内自动更新**。

```
产品名:   haxsd byok
identifier: dev.haxsd.byok
版本:     1.0.1
更新地址: https://github.com/haxsd/haxsd-byok/releases/latest/download/latest.json
```

---

## 二、目录结构即架构

```
D:\cursor-byok\byok-dev\haxsd-byok\                    ← 产品仓库（haxsd/haxsd-byok，分支 main）
├─ server\src\
│  ├─ cursor\          接管 Cursor（改其配置 + 本地 CA）
│  ├─ devin\           Devin 网关，10 个文件 3700+ 行
│  │  ├─ gateway.rs    621 行  三端口 Connect 网关
│  │  ├─ catalog.rs    443 行  请求 → 模型库的映射
│  │  ├─ host_patch.rs 508 行  给 Devin 宿主打补丁指向本地端口
│  │  └─ host_detect.rs 233 行 自动定位 Devin 安装位置
│  ├─ control\         前端唯一的 HTTP 边界 /__byok-api__/api/*
│  ├─ store\           SQLite（统计 + 两侧配置）
│  └─ provider\        模型提供方调用
├─ apps\desktop\src\
│  ├─ features\models\   模型库（共享）
│  ├─ features\devin\    Devin 页
│  ├─ features\home\     统计仪表盘（共享）
│  ├─ features\settings\ 设置
│  ├─ shell\             侧边栏、页面骨架
│  ├─ shared\ui\         共享组件
│  └─ styles\            主题与排版 token
└─ apps\desktop\src-tauri\   Tauri 外壳 + webview2\WebView2Loader.dll
```

### 三条不可破坏的边界（用户明确要求）

| 要求 | 落地方式 |
|---|---|
| Cursor 与 Devin 互不影响 | 各自独立的网关、端口、配置、界面 |
| 只共享统计数据 | 统计表与 `/api/overview`、`/api/llm-calls` 只有一个 |
| 共享模型库 | 一张 `model_configs` 表；Devin 侧通过**绑定**引用 |

Devin 侧不改模型库本身，而是建立「Devin 模型 UID → 模型库条目」的映射（`DevinBindingKind`：`standard` / `context_compression`）。

---

## 三、当前状态

### 已装到用户机器上并验证

```
安装位置: C:\Users\Administrator\AppData\Local\haxsd byok
版本:     1.0.8（机器上装的就是这一版，应用内「检查更新」实测可用）
数据目录: C:\Users\Administrator\.haxsd-byok-devin-v3\haxsd-byok.db   ← 用户数据在这里
```

### 已发布

```
Release:   haxsd-byok-v1.0.8（Latest），2026-09-23
产物:      haxsd.byok_1.0.8_x64-setup.exe / .sig / latest.json
更新地址:  https://github.com/haxsd/haxsd-byok/releases/latest/download/latest.json
```

**`updater:default` 必须留在 `apps/desktop/src-tauri/capabilities/default.json` 里。**
从 1.0.1 到 1.0.7 的每个版本都漏了这一项：更新卡片读得到版本号，点「检查更新」却
只会报 `Command plugin:updater|check not allowed by ACL`——权限缺失在运行前完全看不
出来（编译、类型检查、界面渲染都正常）。**凡是新增 Tauri 插件调用，都要同时检查
capabilities**，否则功能是静默不可用的。

因为 1.0.7（含更早版本）自己查不了更新，**装过这些版本的用户必须手动装一次 1.0.8**；
从 1.0.8 起应用内更新才真正可用（1.0.6 → 1.0.7 的完整链路已实测：检查 → 下载 →
验签 → 安装 → 自动重启）。

**产品仓库必须保持公开**，否则应用内更新会 404：更新器请求时不带凭证，私有仓库的
release 资源不接受匿名下载（实测私有 404 / 公开 302）。签名链已验证过，公钥与签名
密钥标识一致。

**`latest.json` 里的下载地址必须是不限流的形式**（`github.com/.../releases/download/<tag>/<asset>`）。
tauri-action 默认写 `api.github.com` 的资源地址，那个**只给匿名请求每小时 60 次**；本机走共享代理出口，
配额被耗光时安装包下载直接 403（清单本身不受影响）。finalize 任务现在会自动改写并检查这一点。

### 仓库

```
产品仓库  https://github.com/haxsd/haxsd-byok.git    分支 main（**公开**，更新通道指向它）
```

本产品的开发与发布都只在本仓库的 `main` 上进行；旧开发分支的内容已归档为
`legacy/devin-router`，逐文件清单见第七节。

### 本机工作区（2026-09-24 清理后）

`D:\cursor-byok\byok-dev\` 下只保留：`haxsd-byok\`（本仓库）、`cursor-byok\`（Cursor BYOK
仓库，原 `cursor-byok-product`，已重命名）、`_logs\`、`.updater-keys\`（签名私钥，勿删）。
已删除：`cursor-byok-devin-router\`（旧开发仓库检出）、`cursor-byok-upstream\`（上游参考
检出，需要时按 `BRANCH_ISOLATION.md` 重新 clone）、`.e2e-devin\`（本地验证工具与截图）、
`.devin-switch\`（厂商路由器逆向材料）、`installer\`（旧安装包）。

### 用户环境中有两个独立产品，不要搞混

```
haxsd byok      ← 本项目（D:\cursor-byok\byok-dev\haxsd-byok）
Cursor BYOK     ← 另一个产品，安装在 D:\cursor-byok\Cursor BYOK，进程 cursor-byok-desktop
```

**用户明确要求：不要影响 Cursor BYOK 的正常使用。** 两者数据目录和端口都不重叠，但有一个真实风险点见第六节。

---

## 四、工具链与硬性约定

```
Rust:    stable-x86_64-pc-windows-gnu（MSVC 未安装）
Shell:   Windows PowerShell 5.1
```

**踩过的坑，必须遵守：**

| 约定 | 原因 |
|---|---|
| 用 `;` 不用 `&&` 串联命令 | PowerShell 5.1 不支持 `&&` |
| 写 `.ps1` 要带 **BOM** | 否则 PS 5.1 按 GBK 读，中文注释变乱码 |
| 写 JSON 用 `[System.IO.File]::WriteAllText($p,$t,(New-Object System.Text.UTF8Encoding($false)))` | `Set-Content -Encoding UTF8` 会带 BOM，破坏 JSON |
| 路径用绝对路径 | 项目在 D 盘 |
| **不要 `Start-Process -RedirectStandardOutput`** | 沙箱下报 `spawn EPERM` |
| 不要用 node 捕获子进程输出 | 同上，改写入文件 |

---

## 五、UI 设计系统（已完成一轮全面升级）

### token 层（`apps/desktop/src/styles/`）

```
_typography.scss   字号 + 行高 + 字重 + 字距（唯一允许定义字体的地方）
_themes.scss       VS Code 风格 token + 半径尺度 + 阴影 + 动效 + 图表色板
globals.scss       全局默认（行高、focus ring、标题字重）
```

**规则**：组件里**不许写死** `font-size` / `font-weight` / `line-height` / `border-radius`，一律用 token。

### 主题

```
midnight        新增，深靛蓝底 + 提亮卡片 + 12px 圆角 + 22px 区块间距
default-dark    原主题（**当前默认**）
default-light   亮色
```

在 `shared/theme/theme.ts` 里切换默认值（`defaultThemeId`）。

### 图表

canvas 图表**无法继承 CSS**，所以色板定义在主题里、由 `features/home/charts/chartTheme.ts` 的 `chartPalette()` 运行时读回。
**新增图表必须走这个函数**，不要写死颜色。

色板是**按主题分别声明**的：`_themes.scss` 里的 `chart-palette` mixin，三个主题各一份
（`--oa-chart-axis` / `--oa-chart-grid` / `--oa-heat-0` 例外，它们引用 `--vscode-*`，会自动跟主题走）。
只在基础 `:root` 里声明一份的后果是**三个主题共用同一套数据色**——曾经如此，亮色主题拿到的
是暗色那套（背景 `#f5f5f5`、热力档位 1 却是 `#1d3a63`）。改色板后要逐主题核对
`--oa-*` 的计算值，确认没有 token 漏改。

---

## 六、必须知道的陷阱（都实际踩过）

| 陷阱 | 真相 |
|---|---|
| **GNU 工具链构建的 Tauri 安装包缺 `WebView2Loader.dll`** | 不打包的话干净机器上启动报「找不到 WebView2Loader.dll 系统错误」。已修为 `tauri.conf.json` 的 `bundle.resources` 带上 `webview2/WebView2Loader.dll`。**改打包配置时别删掉它。** |
| **应用是单实例的** | 已装应用在跑时，第二个副本会被自己踢掉。要验证源码改动，只能另起独立 server 进程服务新构建的前端 |
| **前端编译进二进制** | 改了前端必须重新 `tauri:build` + 安装才能在应用里看到；光 build 前端不够 |
| **`cursor_takeover_enabled` 缺席时默认是 `false`** | 数据库里没有这一行**不会**接管：接管会改用户的 Cursor 配置并强制结束编辑器，必须是用户明确选过的（`store/settings.rs` 有测试锁住）。当前库里显式写入 `false`。**改这一行的语义前先想清楚：默认接管会让「只是装了这个软件」变成一次对用户编辑器的操作** |
| **i18n 插件强制静态字面量** | `t()` 参数必须是字符串字面量，不能是变量，也不能传 JSX。违反会**构建失败** |
| **i18n 缺译文会构建失败** | 加新文案后跑 `npm run i18n:scan`，填 `en-US.json` 里的空词条 |
| **GitHub 推送偶发 `SSL_ERROR_SYSCALL`** | 重试即可 |
| **`github.com` 直连不通 ≠ GitHub 不通：先查系统代理，再动手** | 这台机器上 `github.com:443` 经常直连超时（`git push`、`curl`、应用内更新全都会卡住或失败），而 `api.github.com` 与 `objects.githubusercontent.com` 直连是通的——`gh` 看起来「能用」只是因为它们走的是后两个域名。**系统代理（Clash/mihomo，`127.0.0.1:7897`）开关会变**：`ProxyEnable` 现在可能是 1 也可能是 0，下结论前**每次都要重新读一次**。`curl` / `git` / `node` **都不读系统代理**，只有显式指定才走。<br>先看代理：`Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings' \| Select-Object ProxyEnable,ProxyServer`<br>下载：`curl.exe -sL -x http://127.0.0.1:7897 -o out.exe <url>`<br>推送：`git -c http.proxy=http://127.0.0.1:7897 push origin main`<br>⚠️ 2026-09-24 就是漏了这一步：代理开着、我却直连，把「下不动安装包」误判成网络故障，白等了一晚上。**再遇到大文件下载失败，先按上面这条命令确认代理，再决定是不是真不通。** |
| **复制式移植从不删除文件** | `tar -x` / `Copy-Item` 只会覆盖与新增。从归档分支移植时，旧副本会留在本仓库（曾残留 `LatencyChart.tsx` / `TokenTrendChart.tsx`）。移植后必须用 blob 哈希比对确认，见第七节 |
| **启用窗口只写在 capabilities 里** | Tauri v2 的前端调用要先过 ACL：漏一项权限，功能**静默不可用**，编译与渲染都正常（`updater:default` 从 1.0.1 漏到 1.0.7，更新卡片一直报 `not allowed by ACL`）。新增插件调用时一并改 `apps/desktop/src-tauri/capabilities/default.json` |
| **`finalize` 任务没有 checkout** | `gh release edit` 无法推断仓库，报 `not a git repository`，草稿不会转正。已修为显式 `--repo "${GITHUB_REPOSITORY}"`。**改这个工作流时别删掉 `--repo`** |
| **更新地址必须能匿名读取** | 产品仓库若改回私有，`releases/latest/download/latest.json` 立刻 404，应用内更新全断 |
| **Cursor 的 `settings.json` 是本机共享资源** | 同机另一个产品（Cursor BYOK）也写它，键名与我们完全相同。**任何"清理"都必须凭归属标记，不能凭内容形状推断**，否则会把对方的配置删掉——已经真实发生过一次，见第十四节 |
| **启动时不要碰用户的其他东西** | `desktop.rs` 的 `setup` 里那一串调用，每一个都在用户开机的路径上。`cleanup_stale_settings()` 曾经无条件删 Cursor 配置；**给它加任何新动作前，先想清楚它会不会碰别人的文件** |
| **只有「打开接管」那一刻会结束 Cursor** | 用户按开关 + 确认框之后才 `taskkill /F /T /IM Cursor.exe`（判定在 `local_app/mod.rs` 的 `should_terminate_cursor`）。**读取状态那条每几秒一次的路径永远不许结束进程**：曾经它会这么做，于是同机另一个软件写回自己的配置后，我们下一次刷新就把用户正在编辑的 Cursor 杀掉。**确认框文案里那句话不要删** |
| **别人写进 `settings.json` 的代理配置不覆盖** | `proxy_configuration_is_foreign()` 比较 `http.proxy` 与我们留下的标记：不一致就说明有人在我们之后改过（兄弟产品会保留不认识的键）。这时读状态不写、不启动代理、不碰进程，只在 Cursor 页报「配置冲突」。要接管必须由用户明确打开开关 |
| **Devin 宿主补丁仍指向 43110/43111/43112** | 网关端口若改动，宿主补丁要重打 |
| **不要启动厂商路由器** | `D:\devin-model-router\...\Devin Model Router.exe` 的 `autoPatch` 会把宿主文件改回 43100 端口，把我们的补丁冲掉 |

---

## 七、与旧开发分支的关系（归档，只读）

`haxsd byok` 最初在 `haxsd/cursor-byok` 的 `feat/devin-router` 分支上开发，那个安排已经结束：

```
归档分支   haxsd/cursor-byok 的 legacy/devin-router @ 52f0a47（只读快照）
工作分支   haxsd/cursor-byok 的 wip/audit-1.0.6 @ cdd3c7e（归档之后的修复，已移植进本仓库）
逐文件清单 D:\cursor-byok\byok-dev\_logs\devin-legacy-inventory.md（归档时的差异清单）
归档检出   本地已删除（2026-09-24 清理）；需要查历史时从远端检出 legacy/devin-router
```

**归档这一代的全部内容已经并入本仓库的 `main`**（提交 `chore: 把归档那一代整体并入 main`
+ `chore: 版本号升到 1.0.7`）。并入后按 git blob 哈希逐文件比对，本仓库与归档只差 5 个文档
（本仓库的 `README.md` / `README-EN.md` / `HANDOFF.md` / `BRANCH_ISOLATION.md` /
`docs/devin-go-live.md` 是两仓库格局下的版本，**不要用归档的覆盖**），另删掉 6 个重做后
不再使用的旧组件（`DevinPath.*`、`CacheHitRateChart.*`、`DataTable.*`）。

因此现在的规矩是：

1. 开发与发布都只在本仓库的 `main` 上进行；
2. 归档**只读**，只用于查历史，不要再从它复制文件进本仓库（内容已经一致）；
3. 校验移植是否干净，用 git blob 哈希逐文件比对，不要比文件内容——行尾差异会伪装成
   内容不同：

```powershell
$src = "<legacy/devin-router 检出目录；本地已删除，需要时先从远端 clone>"
$new = "D:\cursor-byok\byok-dev\haxsd-byok"
$files = git -C $src ls-files | Where-Object { $_ -notlike ".github/*" }
$keep = @("README.md","README-EN.md","HANDOFF.md","BRANCH_ISOLATION.md","docs/devin-go-live.md")
foreach ($f in $files) {
  if ($keep -contains $f) { continue }
  if ((git -C $src hash-object -- $f) -ne (git -C $new hash-object -- $f)) { "内容不同: $f" }
}
```

⚠️ 复制式移植（`tar -x`、`Copy-Item`）只覆盖和新增、**从不删除**，旧副本会留在本仓库里：
曾经因此残留过 `LatencyChart.tsx` / `TokenTrendChart.tsx`。两侧文件数相同时也可能一边多一个、
一边少一个，所以比对结果里"多出来的文件"要按清单逐个确认是否该删，**不要只看数量**。

---

## 八、验证手段（不要靠眼睛）

`D:\cursor-byok\byok-dev\.e2e-devin\` 曾是一套本地验证工具（**不是产品代码**，不参与构建），
**已在 2026-09-24 的工作区清理中整体删除**。下表保留工具的用途记录，需要时按它重建：

| 工具 | 用途 |
|---|---|
| `tools/preview-ui.ps1` | **看新 UI 用这个**：独立进程服务工作区构建的前端 + 真实数据库，不安装、不影响已运行应用 |
| `tools/measure-layout.mjs` | 通过 CDP 在**真实视口**量元素几何。**判断布局必须用它** |
| `tools/measure-typography.mjs` | 量实际生效的字号/行高/字重 |
| `tools/measure-bars.mjs` | 量 canvas 图表的柱子几何（位置、数量、每根的高度）。注意网格线会横跨所有列，会干扰"找最高墨迹"的判读 |
| `tools/measure-theme-tokens.mjs` | 量三个主题下 `--oa-*` 色板的**计算值**，直接列出哪些 token 不随主题变化 |
| `tools/zoom-shot.mjs` | 把页面某块区域按高倍率截出来。**canvas 里的文字（坐标轴标签等）只能这样读**，全页缩略图会看错 |
| `tools/render-theme.mjs` | 三个主题各渲染一张截图（通过 localStorage 种入主题，和真实启动路径一致） |
| `tools/set-theme.mjs` | 把主题设回指定值或清掉；截图工具种过主题后用它复位 |
| `tools/verify-update-signature.mjs` | 用应用里编译进去的公钥验已发布的安装包签名，**不需要私钥**。每次发版后都该跑 |
| `tools/perf-probe.mjs` | 页面加载 + API 调用计数 |
| `tools/render-app.mjs` | 渲染已安装应用自己的前端并 dump 结构 |
| `tools/capture-window.py` | 截取真实窗口（需 Pillow 的 python） |
| `tools/list-windows.py` | 枚举进程窗口（判断窗口是否真的显示出来） |
| `tools/inspect-db.py` / `dump-table.py` / `show-setting.py` | 读数据库与设置（排查配置问题） |
| `tools/set-takeover-off.py` | 显式关闭 Cursor 接管开关 |
| `tools/serve-dist.py` | 按 Vite base 提供 dist（普通静态服务器会 404 导致空白页） |

**CDP 量测要给 URL 加 cache-buster。** 这不是可选项：preview server 会继续把浏览器
已持有的旧 bundle 发给你，`Network.setCacheDisabled` 也拦不住，于是**重新构建后的前端会被量成
"没有任何变化"**——这个假阴性已经骗过一次（误判"主题色板没生效"）。自己写脚本时要照做。

**血泪教训：我从截图误判布局两次、误判请求归因一次。缩略图不能用来判断布局，必须用测量脚本。** 例如「状态项被挤成单列」的观感被 CDP 实测推翻——实际是 5 列 × 235px。

### 标准验证流程

```powershell
# 前端
cd D:\cursor-byok\byok-dev\haxsd-byok\apps\desktop
npm run check          # tsc + vite + i18n 校验，必须绿灯

# 后端
cd D:\cursor-byok\byok-dev\haxsd-byok
cargo fmt --all -- --check
cargo test --workspace --exclude haxsd-byok-desktop   # 本地必须排除桌面 crate（GNU 运行时问题）
```

> 桌面 crate 在本地跑测试会 `0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND`（GNU/MCF 运行时问题），CI 的 MSVC 能过。这是环境问题，不是代码问题。

### 构建与安装

```powershell
cd D:\cursor-byok\byok-dev\haxsd-byok
npm --prefix apps/desktop run tauri:build -- --bundles nsis
# 产物: target\release\bundle\nsis\haxsd byok_<版本>_x64-setup.exe

Get-Process -Name haxsd-byok-desktop | Stop-Process -Force   # 必须先停
Start-Process "target\release\bundle\nsis\haxsd byok_<版本>_x64-setup.exe" -ArgumentList "/S" -Wait
Start-Process "D:\cursor-byok\haxsd-byok\haxsd-byok-desktop.exe"
```

**本地构建会报签名失败**（`A public key has been found, but no private key`）——这是预期的，签名私钥只在 CI 里。安装包仍然产出可用，只是没有 `.sig`。

### 安装位置（2026-09-25 起固定为 D 盘）

本机的安装位置是 **`D:\cursor-byok\haxsd-byok`**（与隔壁 Cursor BYOK 的 `D:\cursor-byok\Cursor BYOK` 同一层），注册表 `HKCU\...\Uninstall\haxsd byok` 的 `InstallLocation` 指向它。要点：

- 静默安装到指定目录：`Start-Process <setup.exe> -ArgumentList "/S", "/D=D:\cursor-byok\haxsd-byok" -Wait`
  —— Tauri 的 NSIS **接受 `/D=`**（测试过），`/D` 必须是最后一个参数且不带引号。
- **更新与重装会自动留在原位置**：安装包先读卸载注册表里的安装位置，有记录就复用 `$INSTDIR`。
  实测：应用装在 D 盘时直接跑 `setup.exe /S`（不带 `/D`，这正是应用内更新器的调用方式），文件仍在
  D 盘、没有在 `%LOCALAPPDATA%` 生成第二份。所以「更新装回原位置」是安装器自身的行为，不需要额外交代。
- 想在别的目录重装：先卸载（会清掉注册表记录），再用 `/S /D=<新目录>` 装一次。

---

## 九、发布流程（需用户确认）

发布会产生**对外可见**的 release，**必须先问用户**。

```
1. 改版本号。**四处**必须一致，少一处就会发布失败或产物对不上：
     apps/desktop/package.json
     apps/desktop/package-lock.json   ← 根 version 和 packages[""].version 两行
     apps/desktop/src-tauri/tauri.conf.json
     apps/desktop/src-tauri/Cargo.toml
     Cargo.lock                       ← haxsd-byok-desktop 那一条（改 Cargo.toml 后
                                         cargo 不一定会自动改写，`cargo check --locked`
                                         能验证你改对了）
     （`npm version 1.0.x --no-git-tag-version` 能正确改前两个文件）
2. 提交并推送到本仓库的 `main`
3. 在本仓库打 tag 并推送：haxsd-byok-v1.0.x
     - tag 名字必须等于 haxsd-byok-v<版本号>
     - tag 指向的提交必须**在本仓库的 origin/main 上**，否则 prepare 任务直接拒绝
4. Release workflow：构建（Windows/MSVC）→ 签名 → 建**草稿** Release
   → **改写 latest.json 的下载地址**（见下）→ finalize 转正为 latest
5. 应用内更新才真正可用（前提：产品仓库公开）
```

**finalize 里有一步不能删：把 `latest.json` 的下载地址改写掉。** tauri-action 写的是
`api.github.com` 资源地址，那个只给匿名请求每小时 60 次配额；配额耗尽时安装包下载 403，
清单却照样能读——**表现为"能查到有新版本，但下载失败"**。改写脚本在工作流里，
不匹配任何资源或改写后仍含 `api.github.com` 都会直接让发布失败。

**版本号必须往上抬。** 更新器只认严格更新的版本：已装 1.0.1 时再发一个 1.0.1，用户那边什么都不会发生。

**签名密钥**（已在 GitHub Secret `TAURI_SIGNING_PRIVATE_KEY` 配好）：

```
D:\cursor-byok\byok-dev\.updater-keys\haxsd-byok.key   ← 私钥，在仓库外
D:\cursor-byok\byok-dev\.updater-keys\haxsd-byok.key.pub
```

> ⚠️ **不要读取或外传私钥内容。丢了它，后续所有签名更新都不可能。**
> 不要在输出里打印私钥或设置后的环境变量。

工作流还会引用 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`，但产品仓库里**没有**这个 secret——
签名能成功，说明这把密钥没有设密码。将来换带密码的密钥时必须补上这个 secret。

**发布后必须验三件事**（发布页看起来正常，更新仍可能是坏的）：

```powershell
# 1. 草稿真的转正了吗
gh release view haxsd-byok-v1.0.x --repo haxsd/haxsd-byok --json isDraft,assets

# 2. 更新地址能匿名读吗（私有仓库会 404）
curl.exe -s -o NUL -w "%{http_code}`n" "https://github.com/haxsd/haxsd-byok/releases/latest/download/latest.json"

# 3. 签名与应用里编译进去的公钥对得上吗（不需要私钥）
#    用 tauri.conf.json 里的 pubkey 校验安装包与 .sig
#    （原先的 verify-update-signature.mjs 已随本地工具集删除，需要时重建）
```

第 3 条尤其值得每次跑：公钥和签名密钥一旦不一致，**每个客户端都会拒绝每一次更新，而发布页上看不出任何异常**。

---

## 十、已经完成的工作（不要重做）

### 功能

- Devin 网关：三端口 loopback、Connect 协议、本地处理 `GetChatMessage`/`GetCliModelConfigs`、其余转发上游
- 宿主自动探测：从 `Devin.exe` 反推 `resources\app\extensions\windsurf\dist\extension.js`，不再要求手填
- 宿主补丁：校验四个版本锚点 + SHA-256 备份，可恢复
- 应用内更新：`tauri-plugin-updater` + 自有仓库的 `latest.json`

### UI（本轮全面升级）

- **Cursor 与 Devin 并列为「接入模块」**，各有状态标签（`已接管`/`未接管`、`运行中`/`待重启`/`未启用`）
- **模型库独立成页** `/models`，不再被 CA 门控拦住（这是用户最早的抱怨点）
- Cursor 页 412 行 → 97 行，只留接管与 CA
- Devin 页三段式：接入状态 / 基础设置 / 高级（折叠，记忆选择）
- 设置页低频卡片折叠 + 记忆
- 视觉基础：排版尺度（行高/字重/字距）、半径尺度、阴影、动效、唯一焦点环
- 卡片顶部高光、表格行高与小型大写表头、仪表盘数值 30px、页面标题 24px
- 图表色板接入主题（原先全硬编码 + 与主题无关的灰色坐标轴）

### 修掉的真实缺陷

1. `CursorCaGate` 读一个已无人提供的 context → 无论 CA 是否就绪都只显示引导文字
2. 模型配置包在 CA 门控内 → 未初始化 CA 就看不到模型
3. 更新卡片只在点击时才读版本 → 页面永远显示「读取中…」
4. 折叠状态不记忆
5. 侧边栏只有 Cursor 有状态标签（违反「并列模块」要求）
6. 导航名与页面标题不一致（`数据概览` vs `概览`）
7. **GNU 构建的安装包缺 `WebView2Loader.dll`** → 干净机器上启动失败
8. 图表颜色硬编码、与主题脱节；日历图还在用 GitHub 绿
9. `HomePage` 两个 effect 依赖了未使用的 `overview`

---

## 十一、待办与已知问题

| 优先级 | 事项 | 说明 |
|---|---|---|
| ~~高~~ | ~~同步产品仓库~~ | 已完成：旧开发分支的存量归档为 `legacy/devin-router`，逐文件清单见第七节 |
| ~~高~~ | ~~发布正式 Release~~ | 已完成：`haxsd-byok-v1.0.3`（Latest），签名链与匿名下载都已实测验证 |
| ~~低~~ | ~~`latest.json` 下载地址走 `api.github.com`~~ | 已修：finalize 阶段改写成不限流的 `github.com/.../releases/download/...`，并在改写失败时中断发布 |
| 中 | 图表**形态**重设计 | 已修：空柱等高、日历数据源、三主题色板、仪表盘硬编码绿色、tooltip 走 token。**形态本身（柱状/热力图）未做** |
| 中 | 日期选择器、命令面板等长尾控件 | 未逐一走查 |
| 低 | 偶发测试 `database is locked` | `newer_run_request_on_one_bidi_stream_replaces_the_active_run` 出现过一次；源仓库 12 次运行未复现。**无复现证据前不要改池配置** |
| 低 | 页面切换过渡动效 | 只做了基础动效（按钮/弹窗/折叠） |
| 低 | 图表的 memo 不依赖主题 | `DailyTokenUsageChart` / `ContributionCalendarChart` 的 `useMemo` / `useLayoutEffect` 依赖里没有主题，主题变了颜色不会重算。**当前不可见**（主题开关只在设置页，切主题时首页已卸载，回来是重新挂载），但如果哪天把主题开关挪到常驻位置就会露出来 |
| 待定 | 把 `midnight` 设为默认主题 | 用户尚未表态 |

### 明确挂起（不要擅自推进）

- **Devin 客户端模型名注入**：让 BYOK 模型名显示在 Devin 自己的模型选择器里。厂商靠改写 `UserStatus` 做到，我已定位到 `exa.seat_management_pb` 的字段表（`cli_model_uids` 是 13 号字段），但要数轮协议侦察且无产品产出。**用户没有确认要做，不要开工。**
- 刷新会重取范围数据：事实存在、原因未查明。除非用户觉得卡，否则不值得投入。

---

## 十二、工作方式约定（用户明确要求）

- **中文回复**，代码注释、提交信息、文档也用中文
- **第一句给结论**，不写开场白、不复述需求、不总结刚做了什么
- **改动小且可逆就做完再汇报**；**改动大**（跨模块、动架构/数据/依赖/不可逆）**先给 3~6 条计划等确认**
- 能从代码/配置/文档查到的，自己去查，不要问用户
- 需要用户拍板的：**给选项 + 推荐哪个 + 理由**
- **改完自己验证**，区分「已验证」和「未验证」，未验证的说清怎么验证
- **不留 TODO 当交付**；做不完的单独列出并说明原因
- 提交前看 `git status`，只提交相关文件（**不要习惯性 `git add -A`**）
- **失败必须显式暴露**，不要吞掉报错
- **不要为了「看起来完整」重构无关代码**
- 不确定就直说「不确定」，不要用模糊措辞盖过去

### 用户对本项目的具体要求

- Cursor 和 Devin 是**两个并列模块，互不影响**（除了统计数据）
- 页面要**美观、便捷**
- **不要影响 Cursor BYOK 的正常使用**
- 不要主动删除/覆盖/重命名用户已有文件；必须做时先说明后果并等确认
- 不要读取或外传密钥类文件

---

## 十三、接手后的建议顺序

1. 跑一遍验证（第八节），确认基线是绿的
2. 起一个独立 server 进程预览当前界面，建立视觉基线
3. 按第十一节的优先级推进

### 快速自检清单

```powershell
# 1. 应用在跑吗
Get-Process -Name haxsd-byok-desktop -EA SilentlyContinue
foreach ($p in 1634,43110,43111,43112) { Get-NetTCPConnection -State Listen -LocalPort $p -EA SilentlyContinue }

# 2. 前端绿灯吗
cd D:\cursor-byok\byok-dev\haxsd-byok\apps\desktop; npm run check

# 3. 后端绿灯吗
cd D:\cursor-byok\byok-dev\haxsd-byok
cargo fmt --all -- --check
cargo test --workspace --exclude haxsd-byok-desktop

# 4. 用户的模型和 Devin 绑定还在吗
#    用 sqlite 工具读 %USERPROFILE%\.haxsd-byok-devin-v3\haxsd-byok.db（原 inspect-db.py 已删除）

# 5. Cursor BYOK 没被影响吗
Get-Process -Name cursor-byok-desktop -EA SilentlyContinue
```

---

## 十四、与 Cursor BYOK 的隔离（硬性要求）

用户明确要求：**本产品的开发与运行都不得影响另一个独立产品 Cursor BYOK 的正常使用。**
这不是"尽量"，是出过一次真实事故之后的硬要求，下面是那次事故的完整记录。

### 出过的事故（2026-09-23）

安装 1.0.2 后应用被自动拉起，**用户正在使用的 Cursor BYOK 立刻失效**（他的 Cursor 失去了模型），
他只能关掉 Cursor BYOK 重启。

根因：两个产品是**同一个代码库分叉出来的**，往 Cursor 的 `APPDATA\Cursor\User\settings.json`
写的是**同样五个键、同样的值、同样指向回环地址**：

```
http.proxy / http.proxyKerberosServicePrincipal / http.proxySupport
cursor.general.disableHttp2 / http.experimental.systemCertificatesV2
```

而 `local_app::settings::clear_stale_managed_settings()` 的判据是**"这三个键在、且 proxy 指向回环"**
就认定"这是我自己留下的旧配置"，然后把五个键**全部删掉**。于是：

- 它分不清那是 Cursor BYOK 的配置还是自己的残留（内容上完全一样）
- 这个调用在 `desktop.rs` 的 `setup` 里**无条件执行**，完全不看 `cursor_takeover_enabled`

时间线（本地时间）：`08:53:49` 我们的 1.0.2 启动 → `08:54:20` 对方的配置被改写 → 用户重启对方恢复。

### 修法：归属必须被记录，不能被推断

`settings.rs` 现在会把自己的配置打上标记键 `haxsd-byok.managedProxy`，
**所有删除动作都以标记为准**；没有标记的文件，无论长得多像，一律不碰。
新增了 5 个回归测试（含"把对方那份配置原样留着"和"我们不写过的条目一律不动"）。

同一个原则适用于以后任何"清理"逻辑：**能证明是自己的，才能删。**

### 同类风险：结束 Cursor 这件事只允许发生在「用户明确打开接管」那一刻

`local_app::apply_takeover(explicit_takeover)` 在配置尚未生效时会调用
`process::terminate_cursor()`，在 Windows 上是 `taskkill /F /T /IM Cursor.exe`——
**强杀所有 Cursor 进程和整棵进程树**，未保存的编辑内容会丢。

判定是 `should_terminate_cursor(explicit, already_ours)`，只有**两件事同时成立**才结束进程：

1. `explicit == true`，即用户按下接管开关并在确认框上点了继续（`set_enabled(true)`）；
   读取状态（`status()`，前端每几秒调一次）恒传 `false`，**任何刷新都不可能结束进程**；
2. 配置还不是我们的（已经是我们的再杀一次只有代价）。

开关前已有确认框（`CursorSettingsPage` 的"开启接管Cursor？"），**那句话不要删**。

另外：`settings.json` 里现在是别人写的代理配置时，读状态连写都不写
（`proxy_configuration_is_foreign()`）——两个软件轮流覆盖同一份配置，只会让用户
两条链路都不稳定，要不要抢过来只能由用户在开关上决定。

### 已经核对过、确认互不干扰的面

| 面 | 状态 |
|---|---|
| 监听端口 | 我们 `1634 / 43110 / 43111 / 43112`；对方 `15524 / 35756`。不重叠 |
| 数据目录 | 我们 `~\.haxsd-byok-devin-v3`；对方 `%LOCALAPPDATA%\dev.cursorbyok.desktop`。不重叠 |
| 安装目录 | 我们 `%LOCALAPPDATA%\haxsd byok`；对方 `%LOCALAPPDATA%\Cursor BYOK` 与 `D:\cursor-byok\Cursor BYOK`。不重叠 |
| productName / identifier | `haxsd byok` / `dev.haxsd.byok`，与对方不同 |
| 窗口标题、托盘提示 | 都是 `haxsd byok` |
| 开机自启 | 我们默认不注册（`tauri_plugin_autostart` 只是初始化，没有自动写注册表）；对方有自启项 |
| Windows 根证书存储 | 我们**只读**打开做检查（`ca\windows.rs`），从不安装证书 |
| Cursor `settings.json` | **曾经冲突，已修**（见上） |
| 应用图标 | **曾经逐字节相同（同一 SHA256），已换**（见下） |
| 卸载器 | 实测不动 `settings.json`（卸载前后哈希一致） |

### 图标必须与对方不同

两个产品的 `icon.ico` / `icon.icns` / `32x32.png` 等**曾经逐字节相同**，任务栏里根本分不清哪个是哪个。
现在是一把钥匙（深靛蓝底 + 长春花蓝），生成方式见
`apps/desktop/src-tauri/icons/generate-source.ps1`。

⚠️ `tauri icon` **不会更新 `linux-*.png`**（那是 Tauri v1 的旧文件名，但仍在 `bundle.icon` 里被引用），
所以换图标时必须跑那个脚本，否则这几个文件会留在旧图上。托盘图标也走 `icons/32x32.png`。

### 改但不彻底的：插件 ID 与内部标识

`server/plugins/build-in/*/plugin.json` 的 id 仍是 `dev.cursorbyok.*`（对方的命名空间）。
**故意没改**：运行时 `~\.haxsd-byok-devin-v3\plugins\installed\` 里已经有同旧 ID 的副本，
只改内置的那份会让同一个插件出现两份（而且那是用户已配好的 OAuth 提供方），需要配套迁移。
副本在我们自己的数据目录里，**不构成跨产品干扰**，所以留作已知的外观问题。

`search/` 的 User-Agent 曾经自称 `CursorBYOK/0.1`，已改为 `haxsd-byok/0.1`（对外可见的品牌泄漏）。
插件运行时的 import map 名（`cursor-byok:plugin` 等）和 Monaco 主题名 `cursor-byok` 属于纯内部标识，
改名要动上百处且收益为零，未动。
