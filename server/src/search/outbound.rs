//! 出网并发闸门:搜索与抓取共用一条"进程内最多 N 个在途请求"的上限。
//!
//! 一次 WebSearch 会同时打 23 个搜索引擎,而一批工具调用可以并发好几个 WebSearch/WebFetch:
//! 服务端因此在没有用户可见反馈的情况下放出 23×N 个出网请求(每个还各自建连接池)。这里把
//! 同时在途的请求数压到一个固定上限,顺带把突发流量摊平;它不改变结果内容,只改变时序。
use std::sync::OnceLock;

use tokio::sync::{Semaphore, SemaphorePermit};

/// 同时在途的搜索/抓取出网请求上限。取值偏小:这是本机桌面应用,出网请求是用户可感知的
/// 副作用(对搜索引擎与目标站点而言),并发 4 已经能让 23 个引擎的扇出明显收敛。
const MAX_OUTBOUND_REQUESTS: usize = 4;

static GATE: OnceLock<Semaphore> = OnceLock::new();

/// 取得一个出网名额,直到请求结束(guard 释放)才归还。
pub(crate) async fn permit() -> SemaphorePermit<'static> {
    GATE.get_or_init(|| Semaphore::new(MAX_OUTBOUND_REQUESTS))
        .acquire()
        .await
        .expect("the outbound gate is never closed")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// 名额用尽后新的请求必须排队(而不是一起放出去)。
    ///
    /// 闸门是进程级的,同进程并行跑的其他搜索用例也在用它,所以这里只断言"拿满之后立刻
    /// 再拿会被挡住",以及"全部释放后能继续前进";队列里具体先醒谁由 tokio 的公平队列决定,
    /// 不做时序假设。
    #[tokio::test]
    async fn the_gate_caps_requests_in_flight() {
        let mut held = Vec::new();
        for _ in 0..MAX_OUTBOUND_REQUESTS {
            held.push(permit().await);
        }

        assert!(
            tokio::time::timeout(Duration::from_millis(50), permit())
                .await
                .is_err(),
            "名额用尽后必须排队"
        );

        held.clear();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), permit())
                .await
                .is_ok(),
            "名额全部释放后必须能继续前进"
        );
    }
}
