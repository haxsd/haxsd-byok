//! Verifies registry ownership follows the transport actor rather than output subscriptions.

#[path = "support/fake_provider.rs"]
mod fake_provider;
#[path = "support/fixtures.rs"]
mod fixtures;

use std::{sync::Arc, time::Duration};

use cursor_server::cursor::{
    conversation::TransportCommand,
    prompting::{PromptAssets, PromptCompiler},
    transport::TransportRegistry,
};

async fn registry() -> (tempfile::TempDir, TransportRegistry) {
    let (directory, store) = fixtures::temp_store().await;
    let assets = PromptAssets::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("prompt/cursor")
            .as_path(),
    )
    .unwrap();
    (
        directory,
        TransportRegistry::new(
            store,
            Arc::new(fake_provider::FakeProvider::default()),
            PromptCompiler::new(assets),
        ),
    )
}

#[tokio::test]
async fn actor_exit_removes_the_matching_transport_and_allows_a_new_generation() {
    let (_directory, registry) = registry().await;
    let first = registry.get_or_create("lifecycle-request").await.unwrap();
    assert!(registry.local("lifecycle-request").await.is_some());

    first.command(TransportCommand::Disconnect).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if registry.local("lifecycle-request").await.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let second = registry.get_or_create("lifecycle-request").await.unwrap();
    assert_eq!(second.request_id(), "lifecycle-request");
    assert!(registry.local("lifecycle-request").await.is_some());
    second.command(TransportCommand::Disconnect).await.unwrap();
}

/// 同一个 request_id 在上一轮用完之后条目会被回收:此后同 id 的 RunSSE 再没有生产者
/// 会唤醒它。等待必须有上界,否则这条连接永久挂着、关服还要吃满 10 秒预算。
#[tokio::test]
async fn waiting_for_a_route_nobody_creates_gives_up_with_not_found() {
    let (_directory, registry) = registry().await;
    // 建好 registry(含 sqlx 连接池)之后再暂停时钟,否则池超时也一起被冻住。
    tokio::time::pause();

    // 兜底上界比 registry 自己的等待上界更晚，所以正常路径仍然由被测代码先返回。
    // 上界一被去掉，虚拟时间会走到这个定时器：回归表现为干净失败，而不是永远挂着
    // （暂停的时钟在没有定时器时不前进，"挂住"才是默认结局）。
    const GUARD_GRACE: Duration = Duration::from_secs(60);
    let error = tokio::select! {
        error = registry.wait_route("recycled-request") => {
            error.expect_err("没有任何生产者时不能无限等待")
        }
        _ = tokio::time::sleep(GUARD_GRACE) => panic!(
            "等待路由没有上界：虚拟时钟已走过 {GUARD_GRACE:?}，这条用例只会在全局超时时挂掉"
        ),
    };

    assert!(
        matches!(error, cursor_server::Error::RunNotFound(_)),
        "应按 run 不存在返回: {error:?}"
    );
}

#[tokio::test]
async fn dropping_an_output_subscription_does_not_remove_the_transport() {
    let (_directory, registry) = registry().await;
    let handle = registry
        .get_or_create("subscription-request")
        .await
        .unwrap();
    let subscription = handle.subscribe();
    drop(subscription);

    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(registry.local("subscription-request").await.is_some());

    handle.command(TransportCommand::Disconnect).await.unwrap();
}
