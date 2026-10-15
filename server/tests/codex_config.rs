//! 只在临时 CODEX_HOME 中验证受管字段和恢复，不读取真实用户凭据。
use cursor_server::codex::config::{apply, inspect, restore, ConfigRoute};
fn route(home: &std::path::Path) -> ConfigRoute {
    ConfigRoute {
        model: "hash".into(),
        base_url: "http://127.0.0.1:43120/v1".into(),
        token: "local-test-token".into(),
        catalog_path: home.join("models.json"),
        context_window: 32768,
    }
}
#[test]
fn applying_and_restoring_preserve_comments_and_unrelated_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = "# retained\nmodel = 'old'\nweb_search = 'live'\n\n[mcp_servers.sample]\ncommand = 'example'\n";
    std::fs::write(&path, original).unwrap();
    assert!(apply(dir.path(), &route(dir.path())).unwrap().applied);
    assert!(inspect(dir.path()).unwrap().applied);
    let applied = std::fs::read_to_string(&path)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    assert_eq!(applied["web_search"].as_str(), Some("disabled"));
    restore(dir.path()).unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}
#[test]
fn restoring_keeps_user_changes_to_unmanaged_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "model = 'old'\n").unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let text =
        std::fs::read_to_string(&path).unwrap() + "\n[mcp_servers.added]\ncommand = 'keep'\n";
    std::fs::write(&path, text).unwrap();
    restore(dir.path()).unwrap();
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("command = 'keep'"));
    assert!(text.contains("model = 'old'"));
    assert!(!text.contains("local-test-token"), "restored TOML: {text}");
}
#[test]
fn foreign_providers_and_modified_managed_fields_are_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "model_provider = 'other'\n").unwrap();
    assert!(apply(dir.path(), &route(dir.path())).is_err());
    std::fs::write(&path, "").unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("43120", "43121");
    std::fs::write(&path, &text).unwrap();
    assert!(inspect(dir.path()).unwrap().conflict);
    assert!(restore(dir.path()).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), text);
}
#[test]
fn absent_config_is_removed_after_restore_and_repeated_restore_is_safe() {
    let dir = tempfile::tempdir().unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    restore(dir.path()).unwrap();
    restore(dir.path()).unwrap();
    assert!(!dir.path().join("config.toml").exists());
}
