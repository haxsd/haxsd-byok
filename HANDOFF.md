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
D:\cursor-byok\byok-dev\cursor-byok-devin-router\      ← 开发仓库（分支 feat/devin-router）
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
版本:     1.0.1（机器上装的仍是这一版；1.0.2 已发布，可应用内升级）
数据目录: C:\Users\Administrator\.haxsd-byok-devin-v3\haxsd-byok.db   ← 用户数据在这里
```

### 已发布

```
Release:   haxsd-byok-v1.0.2（Latest），2026-09-22
产物:      haxsd.byok_1.0.2_x64-setup.exe / .sig / latest.json
更新地址:  https://github.com/haxsd/haxsd-byok/releases/latest/download/latest.json
```

**产品仓库必须保持公开**，否则应用内更新会 404：更新器请求时不带凭证，私有仓库的
release 资源不接受匿名下载（实测私有 404 / 公开 302）。签名链已用
`.e2e-devin\tools\verify-update-signature.mjs` 验过，公钥与签名密钥标识一致。

### 仓库

```
开发仓库  https://github.com/haxsd/cursor-byok.git   分支 feat/devin-router（私有）
产品仓库  https://github.com/haxsd/haxsd-byok.git    分支 main（**公开**，更新通道指向它）
```

两个仓库的**内容**已逐文件一致（git blob 哈希比对）：610 个文件，只有 4 个文档不同。

### 用户环境中有两个独立产品，不要搞混

```
haxsd byok      ← 本项目（D:\cursor-byok\byok-dev\cursor-byok-devin-router）
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
是暗色那套（背景 `#f5f5f5`、热力档位 1 却是 `#1d3a63`）。改色板后跑
`tools/measure-theme-tokens.mjs`，它会直接列出哪些 token 不随主题变化。

---

## 六、必须知道的陷阱（都实际踩过）

| 陷阱 | 真相 |
|---|---|
| **GNU 工具链构建的 Tauri 安装包缺 `WebView2Loader.dll`** | 不打包的话干净机器上启动报「找不到 WebView2Loader.dll 系统错误」。已修为 `tauri.conf.json` 的 `bundle.resources` 带上 `webview2/WebView2Loader.dll`。**改打包配置时别删掉它。** |
| **应用是单实例的** | 已装应用在跑时，第二个副本会被自己踢掉。要验证源码改动必须走 `.e2e-devin/tools/preview-ui.ps1`，它用独立 server 进程服务新构建的前端 |
| **前端编译进二进制** | 改了前端必须重新 `tauri:build` + 安装才能在应用里看到；光 build 前端不够 |
| **`cursor_takeover_enabled` 默认是 `true`** | 数据库里没有这一行时默认开启；一旦用户初始化 CA，启动就会去改 **Cursor 的配置**。当前库里已显式写入 `false`。**动这块前先确认这行还在**（`server/src/store/settings.rs` 的 `cursor_takeover_enabled`） |
| **i18n 插件强制静态字面量** | `t()` 参数必须是字符串字面量，不能是变量，也不能传 JSX。违反会**构建失败** |
| **i18n 缺译文会构建失败** | 加新文案后跑 `npm run i18n:scan`，填 `en-US.json` 里的空词条 |
| **GitHub 推送偶发 `SSL_ERROR_SYSCALL`** | 重试即可 |
| **git 自己不走系统代理** | 两个仓库都**没有**配 `http.proxy`。系统代理在 `127.0.0.1:7897`（Clash/mihomo），`gh` 和 PowerShell 会自动用，**git 不会**：直接推送会报 `Failed to connect to github.com port 443`。推送时显式带上 `-c http.proxy=http://127.0.0.1:7897`，或在仓库里配上 |
| **`tar -x` 从不删除文件** | 第七节的同步方式只会覆盖/新增。开发仓库删掉的文件会在产品仓库里留下旧副本（已遇到一次）。同步后必须用 blob 哈希比对确认，见第七节 |
| **`finalize` 任务没有 checkout** | `gh release edit` 无法推断仓库，报 `not a git repository`，草稿不会转正。已修为显式 `--repo "${GITHUB_REPOSITORY}"`。**改这个工作流时别删掉 `--repo`** |
| **更新地址必须能匿名读取** | 产品仓库若改回私有，`releases/latest/download/latest.json` 立刻 404，应用内更新全断 |
| **Devin 宿主补丁仍指向 43110/43111/43112** | 网关端口若改动，宿主补丁要重打 |
| **不要启动厂商路由器** | `D:\devin-model-router\...\Devin Model Router.exe` 的 `autoPatch` 会把宿主文件改回 43100 端口，把我们的补丁冲掉 |

---

## 七、两个仓库的同步（接手后第一件事）

产品仓库落后 6 个提交。同步方式（保持产品仓库自己的 `.github` 和 4 个文档不变）：

```powershell
$src = "D:\cursor-byok\byok-dev\cursor-byok-devin-router"
$new = "D:\cursor-byok\byok-dev\haxsd-byok"
$tar = Join-Path $env:TEMP "byok-sync.tar"
git -C $src archive --format=tar -o $tar HEAD
tar -x -f $tar -C $new --exclude ".github"
Remove-Item $tar -Force
Set-Location $new
# 恢复产品仓库自己的文档与工作流
git checkout -- BRANCH_ISOLATION.md README-EN.md README.md docs/devin-go-live.md
git add -A
git -c user.name="haxsd" -c user.email="haxsd@users.noreply.github.com" commit -m "..."
git push origin main
```

**校验同步结果**（必须用 git blob 哈希，不能直接比文件——行尾差异会伪装成内容不同）：

```powershell
$src = "D:\cursor-byok\byok-dev\cursor-byok-devin-router"
$new = "D:\cursor-byok\byok-dev\haxsd-byok"
$files = git -C $src ls-files | Where-Object { $_ -notlike ".github/*" }
foreach ($f in $files) {
  if ((git -C $src hash-object -- $f) -ne (git -C $new hash-object -- $f)) { "内容不同: $f" }
}
```

预期只有 4 个文件不同：`BRANCH_ISOLATION.md`、`README-EN.md`、`README.md`、`docs/devin-go-live.md`（产品仓库地址不同，有意为之）。

⚠️ **`tar -x` 只会覆盖和新增，不会删除。** 开发仓库删掉的文件会在产品仓库里留下旧副本，
"只差 4 个文档" 就不再成立。曾经因此残留过 `LatencyChart.tsx` / `TokenTrendChart.tsx`。
所以每次同步后都要跑上面这段比对，**不要只看文件数对不对**：两侧数量可能相同（一边多一个、
一边少一个），哈希比对才看得出来。另外两侧文件数不同时，用
`Compare-Object (git ls-tree -r HEAD) ...` 可以直接列出差集。

---

## 八、验证手段（不要靠眼睛）

`.e2e-devin/` 下是本地验证工具，**不是产品代码**，不参与构建：

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

**CDP 工具都会自动给 URL 加 cache-buster。** 这不是可选项：preview server 会继续把浏览器
已持有的旧 bundle 发给你，`Network.setCacheDisabled` 也拦不住，于是**重新构建后的前端会被量成
"没有任何变化"**——这个假阴性已经骗过一次（误判"主题色板没生效"）。工具里已经有 `cacheBust()`，
自己写新脚本时要照做。

**血泪教训：我从截图误判布局两次、误判请求归因一次。缩略图不能用来判断布局，必须用测量脚本。** 例如「状态项被挤成单列」的观感被 CDP 实测推翻——实际是 5 列 × 235px。

### 标准验证流程

```powershell
# 前端
cd D:\cursor-byok\byok-dev\cursor-byok-devin-router\apps\desktop
npm run check          # tsc + vite + i18n 校验，必须绿灯

# 后端
cd D:\cursor-byok\byok-dev\cursor-byok-devin-router
cargo fmt --all -- --check
cargo test --workspace --exclude haxsd-byok-desktop   # 本地必须排除桌面 crate（GNU 运行时问题）
```

> 桌面 crate 在本地跑测试会 `0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND`（GNU/MCF 运行时问题），CI 的 MSVC 能过。这是环境问题，不是代码问题。

### 构建与安装

```powershell
cd D:\cursor-byok\byok-dev\cursor-byok-devin-router
npm --prefix apps/desktop run tauri:build -- --bundles nsis
# 产物: target\release\bundle\nsis\haxsd byok_1.0.1_x64-setup.exe

Get-Process -Name haxsd-byok-desktop | Stop-Process -Force   # 必须先停
Start-Process "target\release\bundle\nsis\haxsd byok_1.0.1_x64-setup.exe" -ArgumentList "/S" -Wait
Start-Process "$env:LOCALAPPDATA\haxsd byok\haxsd-byok-desktop.exe"
```

**本地构建会报签名失败**（`A public key has been found, but no private key`）——这是预期的，签名私钥只在 CI 里。安装包仍然产出可用，只是没有 `.sig`。

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
2. 提交并推送到**产品仓库的 main**（不是开发仓库），再同步一次，见第七节
3. 在产品仓库上打 tag 并推送：haxsd-byok-v1.0.x
     - tag 名字必须等于 haxsd-byok-v<版本号>
     - tag 指向的提交必须**在产品仓库的 origin/main 上**，否则 prepare 任务直接拒绝
     - 开发仓库也有一份同名工作流，但它的 origin/main 是别的东西，**不要在开发仓库打这个 tag**
4. Release workflow：构建（Windows/MSVC）→ 签名 → 建**草稿** Release → finalize 转正为 latest
5. 应用内更新才真正可用（前提：产品仓库公开）
```

**版本号必须往上抬。** 更新器只认严格更新的版本：已装 1.0.1 时再发一个 1.0.1，用户那边什么都不会发生。

**签名密钥**（已在 GitHub Secret `TAURI_SIGNING_PRIVATE_KEY` 配好）：

```
D:\cursor-byok\byok-dev\.updater-keys\haxsd-byok.key   ← 私钥，在仓库外
D:\cursor-byok\byok-dev\.updater-keys\haxsd-byok.pub
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
node D:\cursor-byok\byok-dev\.e2e-devin\tools\verify-update-signature.mjs `
  D:\cursor-byok\byok-dev\haxsd-byok\apps\desktop\src-tauri\tauri.conf.json `
  <下载的安装包> <下载的 .sig>
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
| ~~高~~ | ~~同步产品仓库~~ | 已完成，两仓库逐文件一致（只差那 4 个文档） |
| ~~高~~ | ~~发布正式 Release~~ | 已完成：`haxsd-byok-v1.0.2`，签名链已验证，应用内更新可用 |
| 中 | 图表**形态**重设计 | 已修：空柱等高、日历数据源、三主题色板、仪表盘硬编码绿色、tooltip 走 token。**形态本身（柱状/热力图）未做** |
| 中 | 日期选择器、命令面板等长尾控件 | 未逐一走查 |
| 低 | 偶发测试 `database is locked` | `newer_run_request_on_one_bidi_stream_replaces_the_active_run` 出现过一次；源仓库 12 次运行未复现。**无复现证据前不要改池配置** |
| 低 | 页面切换过渡动效 | 只做了基础动效（按钮/弹窗/折叠） |
| 低 | 图表的 memo 不依赖主题 | `DailyTokenUsageChart` / `ContributionCalendarChart` 的 `useMemo` / `useLayoutEffect` 依赖里没有主题，主题变了颜色不会重算。**当前不可见**（主题开关只在设置页，切主题时首页已卸载，回来是重新挂载），但如果哪天把主题开关挪到常驻位置就会露出来 |
| 低 | `latest.json` 里的下载地址走 `api.github.com` | tauri-action 生成的是 API 资源地址，受匿名 60 次/小时的限制。本机走共享代理出口，实测被限流时 403（配额恢复后正常）。要彻底免疫，需要在 finalize 阶段把 url 改写成 `github.com/.../releases/download/<tag>/<asset>` 形式（不限流） |
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
2. 跑 `tools/preview-ui.ps1` 看一眼当前界面，建立视觉基线
3. 按第十一节的优先级推进

### 快速自检清单

```powershell
# 1. 应用在跑吗
Get-Process -Name haxsd-byok-desktop -EA SilentlyContinue
foreach ($p in 1634,43110,43111,43112) { Get-NetTCPConnection -State Listen -LocalPort $p -EA SilentlyContinue }

# 2. 前端绿灯吗
cd D:\cursor-byok\byok-dev\cursor-byok-devin-router\apps\desktop; npm run check

# 3. 后端绿灯吗
cd D:\cursor-byok\byok-dev\cursor-byok-devin-router
cargo fmt --all -- --check
cargo test --workspace --exclude haxsd-byok-desktop

# 4. 用户的模型和 Devin 绑定还在吗
python D:\cursor-byok\byok-dev\.e2e-devin\tools\inspect-db.py "$env:USERPROFILE\.haxsd-byok-devin-v3\haxsd-byok.db"

# 5. Cursor BYOK 没被影响吗
Get-Process -Name cursor-byok-desktop -EA SilentlyContinue
```
