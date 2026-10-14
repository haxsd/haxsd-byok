# 故障排查

## 更新后证书一直「读取中」、模型与统计不刷新

1.0.17 曾在大数据库上出现历史清理占用写锁、代理启动等待同一写锁的问题。界面又把多个状态读取合并为一次刷新，导致已有模型和证书都无法显示。**1.0.18 已修复该启动阻塞原因。**

1. 在「系统设置 → 软件更新」确认版本；界面无法操作时，退出应用，从 [Releases](https://github.com/haxsd/haxsd-byok/releases/latest) 下载新版安装包覆盖安装。
2. 重新打开应用，检查模型数量、证书与接管状态，再发起一次客户端请求并查看「调用」页。
3. 若新版仍出现相同现象，保存应用版本、客户端版本、发生时间和脱敏错误信息，提交 Issue。不要删除数据库、重复生成 CA 或上传整个数据目录。

这是已修复的后端阻塞原因；界面的请求超时和局部刷新能力仍有改进空间。安装、备份和恢复见 [用户指南](./user-guide.md)。

## Cursor 报 `Failed to establish a socket connection to proxies: PROXY 127.0.0.1:<BYOK 代理端口>`

### 现象

提交聊天后 Cursor 报错：

```
[internal] Failed to establish a socket connection to proxies: PROXY 127.0.0.1:<端口>
```

同一时刻 `renderer.log` 中多个子系统（pricing、privacy mode、metadata sync、插件市场等）报同一条错误。此时 BYOK 本体、管理端口、证书与模型配置均正常，用 `curl` 直接测该代理端口也能连通。

### 成因

底层是 Windows 回环地址偶发 SYN 丢失或延迟。Node 的 libuv 在 Windows 上对回环地址显式关闭 SYN 重传（`src/win/tcp.c` 中 `MaxSynRetransmissions = TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS`），第一发 SYN 未被及时处理即失败，耗时约等于 `MinRto`（约 300ms）。Cursor 基于 Node/Electron，因此把一次底层抖动放大成大面积硬错误；而 `curl`（schannel）与 .NET 会重传 SYN，同样的抖动下它们表现正常，容易误判为「端口没问题」。

### 处理

1. 在 BYOK 设置页把代理端口换成另一个未被占用的端口并重启 BYOK，Cursor 的 `http.proxy` 会自动跟随。
2. 若复发，检查 Npcap 驱动状态（`Get-Service npcap`）；重载驱动（管理员执行 `sc stop npcap` / `sc start npcap`）可清掉卡住的运行时状态。
3. 自检必须用 Node 并发探测回环端口；`curl` 和 .NET 会重传 SYN，暴露不出该问题。

### 状态与注意

- 可恢复，但根因尚未最终确认（主要嫌疑是较老版本的 Npcap 与当前 Windows 版本的组合）；改端口不属于已验证的修复手段。
- 不要把 Cursor 的 `http.proxy` 指向 Clash 等外部代理来绕开该错误，那会使 BYOK 的本地接管失效。
- 详细证据链、时间线与复现步骤见本机记录（未纳入仓库）。

## Cursor 里看不到官方模型，或账号显示成「本地账号」

### 现象

模型列表里只有模型库里的模型，看不到官方模型；Cursor 的账号菜单显示 `cursor@ai.com` / “Cursor Local”。

### 成因

客户端在用本应用注入的本地账号，或者干脆没有登录。本地账号不是官方账号，携带它的请求会被官方接口拒绝（401），官方模型目录抓取失败后，列表就只剩本机的模型。这不是官方模型被移除，也不是开关没生效。

另外，「本地」模式本来就只列模型库里的模型——要同时用官方模型，需要「混合」模式。

### 处理

1. 打开「Cursor」页，切到「混合」。这一步会清除本应用注入的本地账号，并结束正在运行的 Cursor。
2. 重新打开 Cursor，退出登录后用自己的账号登录。
3. 回到「Cursor」页确认这两行：「Cursor 账号：你的账号（…）」和「官方模型目录：正常」。之后模型列表里会同时出现官方模型和你的模型。

「官方模型目录」显示失败时，页面会给出原因：401 通常是账号未登录或登录已过期；网络类原因重复出现时先检查本机到官方服务的连通性。

## 混合/官方模式里官方模型报错或超时，本机模型却正常

### 现象

- 官方模型（或整条 Cursor 请求）报错、超时、连接失败；同一时间本机模型一切正常。
- 出网方式在设置页里是「直连」。

### 成因

「直连」是**全局**的：它同时关掉转发官方请求那条路的代理。官方模型要出我们这一跳到境外
上游，直连下必然连不上；本机（国内）模型不受影响，所以看起来像"只有官方模型坏了"。

### 处理

- 要用官方模型：把出网方式改成「跟随系统代理」（推荐，把分流交给代理软件的规则）或
  「自定义」并填一个能出境的地址。
- 只用本机模型：保持「直连」即可，页面不会再提示。
- 顺带注意：「自定义」只支持一个地址、不按域名分流；填了境外节点，国内模型的请求也会绕出去。

## `cargo test --workspace --all-targets` 在桌面 crate 上失败（`0xc0000139`）

### 现象

`cargo test --workspace --all-targets` 走到最后一个目标时失败：

```
Running unittests src\lib.rs (target\debug\deps\haxsd_byok_desktop-<hash>.exe)
error: test failed, to rerun pass `-p haxsd-byok-desktop --lib`
  process didn't exit successfully: ... (exit code: 0xc0000139, STATUS_ENTRYPOINT_NOT_FOUND)
```

其余所有目标（`cursor-server` 的 11 个套件、`semble-*`）全部通过，失败只发生在
`haxsd-byok-desktop` 的单元测试二进制上。

### 已排除的原因

| 假设 | 证据 |
| --- | --- |
| 构建产物过期/损坏 | `cargo clean` 后全量重编（23.8 GiB 清空重建），新二进制仍然报同一个错 |
| 重链接即可 | 删掉该 exe 触发单独重链接，哈希变化，报错不变 |
| 缺少 `libmcfgthread-2.dll` | 用 C 写探针 `LoadLibrary` + `GetProcAddress`，8 个待解析符号全部返回有效地址；exe 所在目录放置该 DLL 也无效 |
| 缺别的 DLL | 逐个核对导入表：KERNEL32 203 项、ntdll、user32、ucrtbase 系、crypt32 等 0 缺失 |
| PATH / 工作目录 | 净化 PATH、换工作目录、把 DLL 放到 exe 旁，全部一致报错 |
| PE 导入表损坏 | `objdump -p` 的导入描述符、导入名表、导出表结构均正常；那 8 个名字在当前 DLL 里都存在且序号一致 |

### 已知线索

该 exe 的导入 hint 序号比当前 DLL 的导出序号小 1~2（例如 import hint 25 / DLL 里是 26），
说明链接它的静态库是在另一套 MCF 头文件下编出来的。按名字解析本应忽略 hint，
所以这解释不了报错，但这是目前唯一未对齐的地方。

### 结论：本机 GNU 工具链的问题，不是代码问题

同一份代码、同一个测试二进制，在 CI 的 MSVC runner 上正常加载并运行：

```
running 1 test
test startup::tests::fatal_report_includes_the_complete_error_chain ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

（CI 运行 `35567454874`，`Desktop Rust (windows-latest)` 的
「Run the desktop unit tests on MSVC」步骤。）

Windows 桌面单元测试现已在 CI 中默认执行，不再依赖仓库变量启用。检查具体版本时仍应核对该步骤实际执行结果，不能把编译检查或被跳过的测试视为通过。

### 处理

- Windows 开发与验证使用 MSVC 工具链及对应的 Visual Studio Build Tools。已经安装该工具链时，可显式运行：

```powershell
cargo +stable-x86_64-pc-windows-msvc test --locked --workspace --all-targets
```

- GNU 环境的原生依赖错误应单独定位；排除桌面后的测试只能证明被执行的部分，不能代替桌面测试。
- 前端验证运行 `npm --prefix apps/desktop run check`，包含类型检查、行为测试与生产构建。
