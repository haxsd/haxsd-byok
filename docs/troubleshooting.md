# 故障排查

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

所以不需要为它换本地工具链：CI 已经用 MSVC 覆盖了这个测试，工作流里那一步就是门禁，
默认由仓库变量 `RUN_DESKTOP_UNIT_TESTS=true` 打开。

### 处理

- 质量闸门用 `cargo test --workspace --exclude haxsd-byok-desktop`；`cargo fmt`、
  `cargo clippy --workspace --all-targets -- -D warnings`、`cargo check` 均不受影响。
- 桌面侧改动靠 `cargo check -p haxsd-byok-desktop` 与 `npm --prefix apps/desktop run check`
  覆盖，二者都通过；这个测试二进制的实际运行由 CI 的 MSVC 步骤负责。
- 本机若想自己跑一遍，需要换 MSVC 目标工具链（`stable-x86_64-pc-windows-msvc`），
  它不依赖 MCF 运行时；但既然 CI 已覆盖，这只是可选。
