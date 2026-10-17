//! 需要真实 store（与它拉起的 transport actor）的用例共用的脚手架。

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::{
    cursor::prompting::{PromptAssets, PromptCompiler},
    model::ModelInvocation,
    provider::{Provider, ProviderStream},
    store::Store,
};

use super::TransportRegistry;

/// 这些用例都不需要模型真的作答，替身保持空实现。
struct IdleProvider;

impl Provider for IdleProvider {
    fn stream(
        &self,
        _invocation: ModelInvocation,
        _cancellation: CancellationToken,
    ) -> ProviderStream {
        Box::pin(futures_util::stream::empty())
    }
}

/// 一个连着临时数据库、挂着空 provider 的 registry。目录要活到用例结束。
pub(crate) async fn test_registry() -> (tempfile::TempDir, TransportRegistry) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::connect(&format!(
        "sqlite://{}",
        directory.path().join("test.db").display()
    ))
    .await
    .unwrap();
    let registry = TransportRegistry::new(
        store,
        Arc::new(IdleProvider),
        PromptCompiler::new(PromptAssets::embedded().unwrap()),
    );
    (directory, registry)
}
