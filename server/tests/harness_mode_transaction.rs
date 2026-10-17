//! 模式切换是"先做副作用，最后写库"：失败时不能留下已被改写的模式。
//!
//! 隔离数据目录下接管被明确拒绝（`reject_isolated_integration`），是这条不变式最
//! 容易复现的失败入口：不需要证书、不碰用户的 Cursor 文件。
//!
//! 环境变量必须在进程级设置，所以这个文件只有一个测试，避免与其他测试互相影响。

use cursor_server::{local_app::CursorHarness, store::Store};

async fn saved_mode(store: &Store) -> Option<String> {
    sqlx::query_scalar("SELECT value_json FROM service_settings WHERE setting_key = 'cursor_mode'")
        .fetch_optional(store.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_failed_mode_switch_leaves_the_saved_mode_unchanged() {
    let data_directory = tempfile::tempdir().unwrap();
    std::env::set_var("HAXSD_BYOK_DATA_DIR", data_directory.path());

    let store_directory = tempfile::tempdir().unwrap();
    let store = Store::connect(&format!(
        "sqlite://{}",
        store_directory.path().join("test.db").display()
    ))
    .await
    .unwrap();
    let harness = CursorHarness::new(store.clone()).unwrap();

    assert_eq!(saved_mode(&store).await, None, "全新数据库没有模式行");

    // 隔离数据目录下不许接管 Cursor：这一步必须失败，而且不能把新模式写进库。
    let error = harness
        .set_mode(cursor_server::store::CursorMode::Local)
        .await
        .expect_err("isolated data directory must refuse the takeover");
    assert!(
        error.to_string().contains("isolated data directory"),
        "{error}"
    );

    assert_eq!(
        saved_mode(&store).await,
        None,
        "失败的模式切换不能改变已保存的模式，否则界面会显示一个没生效的开关"
    );

    std::env::remove_var("HAXSD_BYOK_DATA_DIR");
}
