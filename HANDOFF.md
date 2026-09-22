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
版本:     1.0.1
数据目录: C:\Users\Administrator\.haxsd-byok-devin-v3\haxsd-byok.db   ← 用户数据在这里
```

### 仓库

```
开发仓库  https://github.com/haxsd/cursor-byok.git   分支 feat/devin-router
产品仓库  https://github.com/haxsd/haxsd-byok.git    分支 main（私有，更新通道指向它）
```

> ⚠️ **接手时必做**：产品仓库落后开发仓库 6 个提交，整轮 UI 升级都没同步过去。
> 同步方式见第七节。

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
| **本机 git 走代理** | 两个仓库都配了 `http.proxy=http://127.0.0.1:7897`，新克隆的仓库要补上，否则推送被重置 |
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
$files = git -C $src ls-files | Where-Object { $_ -notlike ".github/*" }
foreach ($f in $files) {
  if ((git -C $src hash-object -- $f) -ne (git -C $new hash-object -- $f)) { "内容不同: $f" }
}
```

预期只有 4 个文件不同：`BRANCH_ISOLATION.md`、`README-EN.md`、`README.md`、`docs/devin-go-live.md`（产品仓库地址不同，有意为之）。

---

## 八、验证手段（不要靠眼睛）

`.e2e-devin/` 下是本地验证工具，**不是产品代码**，不参与构建：

| 工具 | 用途 |
|---|---|
| `tools/preview-ui.ps1` | **看新 UI 用这个**：独立进程服务工作区构建的前端 + 真实数据库，不安装、不影响已运行应用 |
| `tools/measure-layout.mjs` | 通过 CDP 在**真实视口**量元素几何。**判断布局必须用它** |
| `tools/measure-typography.mjs` | 量实际生效的字号/行高/字重 |
| `tools/perf-probe.mjs` | 页面加载 + API 调用计数 |
| `tools/render-app.mjs` | 渲染已安装应用自己的前端并 dump 结构 |
| `tools/capture-window.py` | 截取真实窗口（需 Pillow 的 python） |
| `tools/list-windows.py` | 枚举进程窗口（判断窗口是否真的显示出来） |
| `tools/inspect-db.py` / `dump-table.py` / `show-setting.py` | 读数据库与设置（排查配置问题） |
| `tools/set-takeover-off.py` | 显式关闭 Cursor 接管开关 |
| `tools/serve-dist.py` | 按 Vite base 提供 dist（普通静态服务器会 404 导致空白页） |

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
1. 确认版本号（apps/desktop/package.json + tauri.conf.json，两处要一致）
2. 打 tag 并推送：haxsd-byok-v1.0.1
3. Release workflow 读取 tag → 构建 → 签名 → 发布 → 生成 latest.json
4. 用户安装后，应用内更新才会真正可用
```

**签名密钥**（已在 GitHub Secret `TAURI_SIGNING_PRIVATE_KEY` 配好）：

```
D:\cursor-byok\byok-dev\.updater-keys\haxsd-byok.key   ← 私钥，在仓库外
D:\cursor-byok\byok-dev\.updater-keys\haxsd-byok.pub
```

> ⚠️ **不要读取或外传私钥内容。丢了它，后续所有签名更新都不可能。**
> 不要在输出里打印私钥或设置后的环境变量。

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
| **高** | 同步产品仓库 | 落后 6 个提交，见第七节 |
| **高** | 发布正式 Release | 需用户确认；完成后应用内更新才可用 |
| 中 | 日期选择器、命令面板等长尾控件 | 未逐一走查 |
| 中 | 图表**形态**重设计 | 目前只统一了颜色，柱状图/热力图还是常规形态 |
| 低 | 偶发测试 `database is locked` | `newer_run_request_on_one_bidi_stream_replaces_the_active_run` 出现过一次；源仓库 12 次运行未复现。**无复现证据前不要改池配置** |
| 低 | 页面切换过渡动效 | 只做了基础动效（按钮/弹窗/折叠） |
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

1. **同步产品仓库**（第七节），确认 609 个文件里只有 4 个文档不同
2. 跑一遍验证（第八节），确认基线是绿的
3. 跑 `tools/preview-ui.ps1` 看一眼当前界面，建立视觉基线
4. **问用户要不要发 Release**（第九节）——这是唯一能解锁应用内更新的动作
5. 然后按第十一节的优先级推进

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
