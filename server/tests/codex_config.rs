//! 只在临时 CODEX_HOME 中验证受管字段和恢复，不读取真实用户凭据。
use cursor_server::codex::{
    config::{
        apply as apply_with, inspect as inspect_with, restore as restore_with, ConfigRoute,
        ConfigStatus,
    },
    RequirementsSource,
};

/// 既有用例只覆盖本地文件所有权：明确注入"没有受管来源"，不触发系统探测。
fn apply(home: &std::path::Path, route: &ConfigRoute) -> cursor_server::Result<ConfigStatus> {
    apply_with(home, route, &RequirementsSource::unmanaged())
}

fn inspect(home: &std::path::Path) -> cursor_server::Result<ConfigStatus> {
    inspect_with(home, &RequirementsSource::unmanaged())
}

fn restore(home: &std::path::Path) -> cursor_server::Result<ConfigStatus> {
    restore_with(home, &RequirementsSource::unmanaged())
}

fn route(home: &std::path::Path) -> ConfigRoute {
    ConfigRoute {
        model: "hash".into(),
        base_url: "http://127.0.0.1:43120/v1".into(),
        token: "local-test-token".into(),
        catalog_path: home.join("models.json"),
    }
}

const FINAL_RECEIPT: &str = ".haxsd-byok-routing.json";
const PENDING_RECEIPT: &str = ".haxsd-byok-routing.pending.json";

fn edit_receipt(path: &std::path::Path, editor: impl FnOnce(&mut serde_json::Value)) {
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    editor(&mut value);
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
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
/// 别家的 provider 不覆盖；受管字段被外部改过只报冲突，撤销永远可用。
#[test]
fn a_foreign_provider_is_refused_and_modified_managed_fields_still_restore() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "model_provider = 'other'\n").unwrap();
    assert!(apply(dir.path(), &route(dir.path())).is_err());
    std::fs::write(
        &path,
        "model = 'old'\n\n[projects.'x']\ntrust_level = 'trusted'\n",
    )
    .unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("43120", "43121");
    std::fs::write(&path, &text).unwrap();
    let status = inspect(dir.path()).unwrap();
    assert!(!status.applied && status.conflict, "{}", status.message);
    assert!(status.restorable, "{}", status.message);
    assert_eq!(status.conflicts.len(), 1, "{:?}", status.conflicts);
    assert_eq!(status.conflicts[0].key, "model_providers.haxsd-byok");
    restore(dir.path()).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("model = 'old'"), "restored TOML: {text}");
    assert!(
        text.contains("trust_level = 'trusted'"),
        "restored TOML: {text}"
    );
    assert!(!text.contains("haxsd-byok"), "restored TOML: {text}");
    let status = inspect(dir.path()).unwrap();
    assert!(
        !status.applied && !status.conflict && !status.restorable,
        "{}",
        status.message
    );
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

/// 写一份临时 requirements 并从它注入；测试永远不会去读真实 ProgramData/HKLM。
fn requirements(home: &std::path::Path, text: &str) -> RequirementsSource {
    let path = home.join("requirements.toml");
    std::fs::write(&path, text).unwrap();
    RequirementsSource::file(path)
}

fn same_provider_requirements(catalog: &std::path::Path) -> String {
    format!(
        "model_provider = \"haxsd-byok\"\nmodel_catalog_json = '{}'\n\n\
         [model_providers.haxsd-byok]\n\
         name = \"haxsd byok\"\n\
         base_url = \"http://127.0.0.1:43120/v1\"\n\
         wire_api = \"responses\"\n\
         requires_openai_auth = false\n\
         supports_websockets = false\n\
         experimental_bearer_token = \"local-test-token\"\n",
        catalog.display()
    )
}

/// 管理员强制另一个 provider：本地文件仍与收据一致，但实际路由不归本应用。
#[test]
fn a_managed_provider_override_is_reported_and_keeps_restore_available() {
    let dir = tempfile::tempdir().unwrap();
    let route = route(dir.path());
    apply_with(dir.path(), &route, &RequirementsSource::unmanaged()).unwrap();
    let source = requirements(dir.path(), "model_provider = \"openai\"\n");

    let status = inspect_with(dir.path(), &source).unwrap();
    assert!(!status.applied, "{}", status.message);
    assert!(status.conflict, "{}", status.message);
    assert!(status.restorable, "{}", status.message);
    assert_eq!(
        status.managed_conflicts.len(),
        1,
        "{:?}",
        status.managed_conflicts
    );
    assert_eq!(status.managed_conflicts[0].field, "model_provider");
    assert_eq!(status.conflict_kind, "managed");
    assert!(status.route_owned, "{}", status.message);
    assert!(status.managed_error.is_none(), "{:?}", status.managed_error);
    assert_eq!(status.managed_scope, "local_system_only");
    assert!(
        status.message.contains("administrator"),
        "{}",
        status.message
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("requirements.toml")).unwrap(),
        "model_provider = \"openai\"\n",
        "核对管理员要求不能改写它"
    );

    let restored = restore_with(dir.path(), &source).unwrap();
    assert!(
        !restored.applied && !restored.restorable,
        "{}",
        restored.message
    );
}

/// 首次接管也被同一条管理员要求挡住：不写 config.toml，也不写收据。
#[test]
fn apply_refuses_a_route_that_administrator_requirements_override() {
    let dir = tempfile::tempdir().unwrap();
    let source = requirements(dir.path(), "model_provider = \"openai\"\n");
    let error = apply_with(dir.path(), &route(dir.path()), &source).unwrap_err();
    assert!(error.to_string().contains("administrator"), "{error}");
    assert!(!dir.path().join("config.toml").exists());
    assert!(!dir.path().join(".haxsd-byok-routing.json").exists());
}

/// 同值要求不是冲突：provider、目录、端点、令牌都相同。
#[test]
fn identical_managed_requirements_keep_the_route_applied() {
    let dir = tempfile::tempdir().unwrap();
    let route = route(dir.path());
    apply_with(dir.path(), &route, &RequirementsSource::unmanaged()).unwrap();
    let source = requirements(dir.path(), &same_provider_requirements(&route.catalog_path));

    let status = inspect_with(dir.path(), &source).unwrap();
    assert!(status.applied, "{}", status.message);
    assert!(!status.conflict, "{}", status.message);
    assert!(
        status.managed_conflicts.is_empty(),
        "{:?}",
        status.managed_conflicts
    );
}

/// 只限制审批/沙箱的要求不改变请求出口。
#[test]
fn approval_and_sandbox_requirements_do_not_block_the_route() {
    let dir = tempfile::tempdir().unwrap();
    apply_with(
        dir.path(),
        &route(dir.path()),
        &RequirementsSource::unmanaged(),
    )
    .unwrap();
    let source = requirements(
        dir.path(),
        "allowed_approval_policies = [\"never\"]\n\
         allowed_sandbox_modes = [\"read-only\"]\n\
         [windows]\nallow_mxc = false\n",
    );

    let status = inspect_with(dir.path(), &source).unwrap();
    assert!(status.applied, "{}", status.message);
    assert!(!status.conflict, "{}", status.message);
    assert!(
        status.managed_conflicts.is_empty(),
        "{:?}",
        status.managed_conflicts
    );
}

/// 同名 provider 被换端点：官方按条目替换，请求不再到本机网关。
#[test]
fn a_managed_override_of_the_same_provider_name_is_a_route_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let route = route(dir.path());
    apply_with(dir.path(), &route, &RequirementsSource::unmanaged()).unwrap();
    let source = requirements(
        dir.path(),
        &format!(
            "model_provider = \"haxsd-byok\"\nmodel_catalog_json = '{}'\n\n\
             [model_providers.haxsd-byok]\n\
             base_url = \"http://127.0.0.1:43121/v1\"\n\
             wire_api = \"responses\"\n\
             requires_openai_auth = false\n\
             experimental_bearer_token = \"local-test-token\"\n",
            route.catalog_path.display()
        ),
    );

    let status = inspect_with(dir.path(), &source).unwrap();
    assert!(!status.applied, "{}", status.message);
    assert_eq!(
        status.managed_conflicts.len(),
        1,
        "{:?}",
        status.managed_conflicts
    );
    assert_eq!(
        status.managed_conflicts[0].field,
        "model_providers.haxsd-byok"
    );
}

/// 目录指针被要求换成别处：客户端读到的目录不再是我们发布的那份。
#[test]
fn a_managed_catalog_override_is_a_route_conflict() {
    let dir = tempfile::tempdir().unwrap();
    apply_with(
        dir.path(),
        &route(dir.path()),
        &RequirementsSource::unmanaged(),
    )
    .unwrap();
    let source = requirements(
        dir.path(),
        "model_catalog_json = 'C:\\ProgramData\\OpenAI\\Codex\\models.json'\n",
    );
    let status = inspect_with(dir.path(), &source).unwrap();
    assert!(!status.applied, "{}", status.message);
    assert_eq!(
        status.managed_conflicts.len(),
        1,
        "{:?}",
        status.managed_conflicts
    );
    assert_eq!(status.managed_conflicts[0].field, "model_catalog_json");
}

/// 文件不存在等于没有约束；注入空文件也不会去探测真实系统策略。
#[test]
fn requirements_are_read_only_from_the_injected_source() {
    let dir = tempfile::tempdir().unwrap();
    apply_with(
        dir.path(),
        &route(dir.path()),
        &RequirementsSource::unmanaged(),
    )
    .unwrap();
    let absent = RequirementsSource::file(dir.path().join("absent.toml"));
    let status = inspect_with(dir.path(), &absent).unwrap();
    assert!(status.applied, "{}", status.message);
    assert!(
        status.managed_conflicts.is_empty(),
        "{:?}",
        status.managed_conflicts
    );

    let empty = requirements(dir.path(), "");
    let status = inspect_with(dir.path(), &empty).unwrap();
    assert!(status.applied, "{}", status.message);
    assert!(
        status.managed_conflicts.is_empty(),
        "{:?}",
        status.managed_conflicts
    );
}

/// 拒绝性检查失败前不能删旧收据：用户手动还原路由并新增 profile 后再次 apply，
/// 旧收据必须还在，随后的 restore 才能把 `web_search` 还回接管前的值。
#[test]
fn a_refused_reapply_keeps_the_previous_receipt_and_restore_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = "model = 'user-choice'\nweb_search = 'live'\n";
    std::fs::write(&path, original).unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();

    let mut doc = std::fs::read_to_string(&path)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    doc.remove("model_provider");
    doc.remove("model_catalog_json");
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        providers.remove("haxsd-byok");
    }
    doc["model"] = toml_edit::value("user-choice");
    doc["profile"] = toml_edit::value("work");
    std::fs::write(&path, doc.to_string()).unwrap();

    let error = apply(dir.path(), &route(dir.path())).unwrap_err();
    assert!(error.to_string().contains("profile"), "{error}");
    assert!(
        dir.path().join(".haxsd-byok-routing.json").exists(),
        "一次被拒绝的 apply 不能删除旧收据"
    );

    let status = inspect(dir.path()).unwrap();
    assert!(!status.applied && !status.conflict, "{}", status.message);
    assert!(status.restorable, "{}", status.message);

    restore(dir.path()).unwrap();
    let doc = std::fs::read_to_string(&path)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    assert_eq!(
        doc.get("web_search").and_then(toml_edit::Item::as_str),
        Some("live"),
        "旧收据里的 web_search 原值必须还能恢复"
    );
    assert_eq!(
        doc.get("profile").and_then(toml_edit::Item::as_str),
        Some("work"),
        "用户新加的 profile 不能被撤销覆盖"
    );
}

/// 首次接管：配置写成功、最终收据没有落盘（写失败或进程中断）后，待提交收据
/// 仍要提供撤销基线。
#[test]
fn a_crash_between_config_and_receipt_keeps_restore_available() {
    let dir = tempfile::tempdir().unwrap();
    apply_with(
        dir.path(),
        &route(dir.path()),
        &RequirementsSource::unmanaged(),
    )
    .unwrap();
    std::fs::rename(
        dir.path().join(".haxsd-byok-routing.json"),
        dir.path().join(".haxsd-byok-routing.pending.json"),
    )
    .unwrap();
    assert!(
        dir.path().join("config.toml").exists(),
        "配置已经写入，故障发生在最终收据落盘"
    );

    let status = inspect_with(dir.path(), &RequirementsSource::unmanaged()).unwrap();
    assert!(!status.applied, "{}", status.message);
    assert!(status.restorable, "{}", status.message);
    assert_eq!(status.conflict_kind, "incomplete_update");
    assert!(status.route_owned, "{}", status.message);

    restore_with(dir.path(), &RequirementsSource::unmanaged()).unwrap();
    assert!(!dir.path().join("config.toml").exists());
    assert!(!dir.path().join(".haxsd-byok-routing.pending.json").exists());
}

/// 最终收据有效时，损坏的待提交文件只是残留：撤销/退出判断不能先解析它。
#[test]
fn a_corrupt_pending_receipt_does_not_block_restore_when_final_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let pending = dir.path().join(PENDING_RECEIPT);
    std::fs::write(&pending, "not json").unwrap();

    let status = inspect(dir.path()).unwrap();
    assert!(status.applied, "{}", status.message);
    assert!(status.restorable, "{}", status.message);

    restore(dir.path()).unwrap();
    assert!(!dir.path().join("config.toml").exists());
    assert!(!dir.path().join(FINAL_RECEIPT).exists());
    assert!(!pending.exists(), "最终收据有效时坏 pending 应被清掉");
}

/// 待提交路径是未知目录时不能递归删除；配置恢复与残留清理失败要区分。
#[test]
fn restore_does_not_recursively_delete_an_unknown_pending_directory() {
    let dir = tempfile::tempdir().unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let pending = dir.path().join(PENDING_RECEIPT);
    std::fs::create_dir(&pending).unwrap();
    std::fs::write(pending.join("unknown.txt"), "keep me").unwrap();

    let error = restore(dir.path()).unwrap_err();
    assert!(
        error.to_string().contains("pending receipt"),
        "清理失败必须单独说明: {error}"
    );
    assert!(
        !std::fs::read_to_string(dir.path().join("config.toml"))
            .unwrap_or_default()
            .contains("haxsd-byok"),
        "配置恢复已经完成，不能把清理失败说成恢复失败"
    );
    assert!(pending.is_dir(), "未知目录不能被递归删除");
    assert!(pending.join("unknown.txt").exists(), "未知目录内容必须保留");
    assert!(
        dir.path().join(FINAL_RECEIPT).exists(),
        "清理失败时保留最终收据，用户修好残留后还能重试"
    );

    // 修复残留后重试：配置已经不存在，不能因为再次删除 config.toml 而失败。
    std::fs::remove_dir_all(&pending).unwrap();
    restore(dir.path()).unwrap();
    assert!(!dir.path().join(FINAL_RECEIPT).exists());
}

/// 待提交收据缺 original：当前基线不完整，不能先报可撤销再删掉用户原值。
#[test]
fn a_pending_receipt_without_original_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let pending = dir.path().join(PENDING_RECEIPT);
    std::fs::rename(dir.path().join(FINAL_RECEIPT), &pending).unwrap();
    edit_receipt(&pending, |receipt| {
        receipt.as_object_mut().unwrap().remove("original");
    });

    assert!(
        inspect(dir.path()).is_err(),
        "缺 original 的 pending 必须失败关闭"
    );
    assert!(
        restore(dir.path()).is_err(),
        "缺 original 的 pending 不能被当成首次接管"
    );
    assert!(
        std::fs::read_to_string(dir.path().join("config.toml"))
            .unwrap()
            .contains("haxsd-byok"),
        "失败关闭不能删除用户当前路由"
    );
}

/// 待提交收据 original 是坏 TOML：加载时就必须拒绝，不能先报 restorable。
#[test]
fn a_pending_receipt_with_invalid_original_toml_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let pending = dir.path().join(PENDING_RECEIPT);
    std::fs::rename(dir.path().join(FINAL_RECEIPT), &pending).unwrap();
    edit_receipt(&pending, |receipt| {
        receipt["original"] = serde_json::json!("model = [");
    });

    assert!(inspect(dir.path()).is_err());
    assert!(restore(dir.path()).is_err());
    assert!(
        std::fs::read_to_string(dir.path().join("config.toml"))
            .unwrap()
            .contains("haxsd-byok"),
        "坏 original 不能导致误删"
    );
}

/// 最终收据缺 original 同样损坏；不能静默退回空基线。
#[test]
fn a_final_receipt_without_original_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let final_receipt = dir.path().join(FINAL_RECEIPT);
    edit_receipt(&final_receipt, |receipt| {
        receipt.as_object_mut().unwrap().remove("original");
    });

    assert!(inspect(dir.path()).is_err());
    assert!(restore(dir.path()).is_err());
    assert!(std::fs::read_to_string(dir.path().join("config.toml"))
        .unwrap()
        .contains("haxsd-byok"));
}

/// 最终收据 original 是坏 TOML：损坏收据失败关闭，不靠 pending 静默降级。
#[test]
fn a_final_receipt_with_invalid_original_toml_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let final_receipt = dir.path().join(FINAL_RECEIPT);
    edit_receipt(&final_receipt, |receipt| {
        receipt["original"] = serde_json::json!("model = [");
    });

    assert!(inspect(dir.path()).is_err());
    assert!(restore(dir.path()).is_err());
    assert!(std::fs::read_to_string(dir.path().join("config.toml"))
        .unwrap()
        .contains("haxsd-byok"));
}

/// 最终收据损坏时不能靠有效 pending 静默降级；失败关闭。
#[test]
fn a_corrupt_final_receipt_does_not_fall_back_to_a_valid_pending() {
    let dir = tempfile::tempdir().unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let final_receipt = dir.path().join(FINAL_RECEIPT);
    let pending = dir.path().join(PENDING_RECEIPT);
    std::fs::copy(&final_receipt, &pending).unwrap();
    edit_receipt(&final_receipt, |receipt| {
        receipt.as_object_mut().unwrap().remove("original");
    });

    assert!(
        inspect(dir.path()).is_err(),
        "最终收据损坏时必须失败关闭，不能把 pending 当成兼容补丁"
    );
    assert!(restore(dir.path()).is_err());
    assert!(std::fs::read_to_string(dir.path().join("config.toml"))
        .unwrap()
        .contains("haxsd-byok"));
}

/// 首次接管的显式 null original 仍是合法基线。
#[test]
fn an_explicit_null_original_is_a_valid_first_takeover_baseline() {
    let dir = tempfile::tempdir().unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let final_receipt = dir.path().join(FINAL_RECEIPT);
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&final_receipt).unwrap()).unwrap();
    assert_eq!(value["original"], serde_json::Value::Null);

    assert!(inspect(dir.path()).unwrap().applied);
    restore(dir.path()).unwrap();
    assert!(!dir.path().join("config.toml").exists());
}

/// 有效收据仍在时，requirements 不可读不能让撤销按钮失效。
#[test]
fn unreadable_managed_requirements_do_not_disable_restore() {
    let dir = tempfile::tempdir().unwrap();
    apply_with(
        dir.path(),
        &route(dir.path()),
        &RequirementsSource::unmanaged(),
    )
    .unwrap();
    let path = dir.path().join("requirements.toml");
    std::fs::write(&path, "model_provider = [\n").unwrap();
    let source = RequirementsSource::file(path);

    let status = inspect_with(dir.path(), &source).expect("状态必须仍可读");
    assert!(!status.applied, "收据与策略不可读不能算已生效");
    assert!(status.restorable, "{}", status.message);
    assert_eq!(status.conflict_kind, "policy_unavailable");
    assert!(status.managed_error.is_some(), "{:?}", status.managed_error);
    assert!(status.route_owned, "{}", status.message);

    restore_with(dir.path(), &source).unwrap();
    assert!(!dir.path().join("config.toml").exists());
}

/// 目录/模型漂移但端点仍归本机：状态必须区分成本地漂移，而不是别家 provider。
#[test]
fn a_local_drift_keeps_route_owned_and_is_not_reported_as_foreign() {
    let dir = tempfile::tempdir().unwrap();
    apply_with(
        dir.path(),
        &route(dir.path()),
        &RequirementsSource::unmanaged(),
    )
    .unwrap();
    let path = dir.path().join("config.toml");
    let mut doc = std::fs::read_to_string(&path)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    doc["model_catalog_json"] = toml_edit::value(
        dir.path()
            .join("drifted-catalog.json")
            .to_string_lossy()
            .as_ref(),
    );
    std::fs::write(&path, doc.to_string()).unwrap();

    let status = inspect_with(dir.path(), &RequirementsSource::unmanaged()).unwrap();
    assert!(!status.applied, "{}", status.message);
    assert!(status.conflict, "{}", status.message);
    assert!(status.route_owned, "{}", status.message);
    assert_eq!(status.conflict_kind, "local_drift");
}

/// 别家 provider 接管：端点不再归本机，状态区分成 foreign。
#[test]
fn a_foreign_provider_is_reported_without_route_ownership() {
    let dir = tempfile::tempdir().unwrap();
    apply_with(
        dir.path(),
        &route(dir.path()),
        &RequirementsSource::unmanaged(),
    )
    .unwrap();
    let path = dir.path().join("config.toml");
    let mut doc = std::fs::read_to_string(&path)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    doc["model_provider"] = toml_edit::value("another-tool");
    std::fs::write(&path, doc.to_string()).unwrap();

    let status = inspect_with(dir.path(), &RequirementsSource::unmanaged()).unwrap();
    assert!(!status.applied, "{}", status.message);
    assert!(status.conflict, "{}", status.message);
    assert!(!status.route_owned, "{}", status.message);
    assert_eq!(status.conflict_kind, "foreign_provider");
}

const LEAK_MARKER: &str = "SECRET_MARKER_DO_NOT_LEAK";

fn assert_sanitized(error: impl std::fmt::Display, context: &str) {
    let text = error.to_string();
    assert!(
        !text.contains(LEAK_MARKER),
        "{context} leaked the sensitive source line: {text}"
    );
}

/// 当前 config 的 TOML 错误不能把出错源行带进 inspect/restore 错误。
#[test]
fn invalid_config_errors_do_not_echo_source_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, format!("token = \"{LEAK_MARKER}\" garbage\n")).unwrap();

    assert_sanitized(inspect(dir.path()).unwrap_err(), "inspect");
    assert_sanitized(restore(dir.path()).unwrap_err(), "restore");
    assert_sanitized(
        apply_with(
            dir.path(),
            &route(dir.path()),
            &RequirementsSource::unmanaged(),
        )
        .unwrap_err(),
        "apply",
    );
}

/// 收据 original 的 TOML 错误不能把出错源行带进 inspect/restore 错误。
#[test]
fn invalid_receipt_original_errors_do_not_echo_source_values() {
    let dir = tempfile::tempdir().unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let receipt = dir.path().join(FINAL_RECEIPT);
    edit_receipt(&receipt, |value| {
        value["original"] = serde_json::json!(format!("token = \"{LEAK_MARKER}\" garbage\n"));
    });

    assert_sanitized(inspect(dir.path()).unwrap_err(), "inspect");
    assert_sanitized(restore(dir.path()).unwrap_err(), "restore");
    assert_sanitized(
        apply_with(
            dir.path(),
            &route(dir.path()),
            &RequirementsSource::unmanaged(),
        )
        .unwrap_err(),
        "apply",
    );
}

/// 管理员 requirements 的 TOML 错误不能把源行带进 managed_error/apply 错误。
#[test]
fn invalid_requirements_errors_do_not_echo_source_values() {
    let dir = tempfile::tempdir().unwrap();
    apply(dir.path(), &route(dir.path())).unwrap();
    let requirements = dir.path().join("requirements.toml");
    std::fs::write(
        &requirements,
        format!("token = \"{LEAK_MARKER}\" garbage\n"),
    )
    .unwrap();
    let source = RequirementsSource::file(requirements);

    let status = inspect_with(dir.path(), &source).unwrap();
    assert_sanitized(status.managed_error.unwrap(), "status.managed_error");
    assert_sanitized(
        apply_with(dir.path(), &route(dir.path()), &source).unwrap_err(),
        "apply",
    );
}
