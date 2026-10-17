//! 冒烟测试用的脚本化插件 worker：不依赖 Deno，只按宿主/worker 的换行 JSON 协议回话。
//!
//! 由 `server/src/plugin/worker.rs` 的用例用裸 `rustc` 编成一个真实可执行文件，然后交给
//! 真正的 `PluginWorker` 去 spawn——这样"进程 + stdio + 协议 + 宿主调用分派"整条链路都被
//! 跑过一遍，而不是在 Rust 里直接调协议解析函数。它刻意只做三件事：
//!   1. 把宿主递过来的 argv 原样记下来（宿主改了运行时参数，这条用例会红）；
//!   2. 收到 `request` 后发一条 `host_call`，把宿主的答复原样带回结果里；
//!   3. 发一个流式 `event` 再发最终 `result`，走完一次完整往返。
//!
//! 只读 stdin、只写 stdout，不联网、不碰文件，因此在任何平台都能跑。

use std::io::{BufRead, Write};

fn main() {
    let argv = std::env::args().skip(1).collect::<Vec<_>>();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(line)) = lines.next() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if string_field(line, "type").as_deref() != Some("request") {
            // `host_result` / `host_error` / `cancel` 之外的输入不该出现；记下来让用例看得见。
            println!(
                "{{\"type\":\"result\",\"id\":\"{}\",\"result\":{{\"unexpected\":\"{}\"}}}}",
                string_field(line, "id").unwrap_or_default(),
                escape(line)
            );
            continue;
        }
        let id = string_field(line, "id").unwrap_or_default();
        let method = string_field(line, "method").unwrap_or_default();

        // 一次宿主调用往返：这个主机不在清单白名单里，宿主必须用 host_error 回绝，
        // 也就是"插件不能自己出网、必须过宿主"这条约束真的生效了。
        println!(
            "{{\"type\":\"host_call\",\"id\":\"host-1\",\"requestId\":\"{id}\",\"method\":\"network.fetch\",\"params\":{{\"url\":\"https://evil.example.net/steal\"}}}}"
        );
        let mut host = String::from("missing");
        while let Some(Ok(line)) = lines.next() {
            let line = line.trim().to_owned();
            match string_field(&line, "type").as_deref() {
                Some("host_error") => {
                    host = format!("error:{}", string_field(&line, "error").unwrap_or_default());
                    break;
                }
                Some("host_result") => {
                    host = "result".to_owned();
                    break;
                }
                _ => continue,
            }
        }

        // 流式事件 + 最终结果：宿主必须把二者分别投递到事件流与结果里。
        println!(
            "{{\"type\":\"event\",\"id\":\"{id}\",\"event\":{{\"type\":\"text-delta\",\"text\":\"stub\"}}}}"
        );
        println!(
            "{{\"type\":\"result\",\"id\":\"{id}\",\"result\":{{\"method\":\"{method}\",\"host\":\"{}\",\"argv\":[{}]}}}}",
            escape(&host),
            argv.iter()
                .map(|argument| format!("\"{}\"", escape(argument)))
                .collect::<Vec<_>>()
                .join(",")
        );
        let _ = std::io::stdout().flush();
    }
}

/// 取一个扁平 JSON 对象的字符串字段：这个桩只面对宿主自己写出的行，字段值不含转义引号。
fn string_field(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
