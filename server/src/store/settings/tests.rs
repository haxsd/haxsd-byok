use std::collections::BTreeMap;

use super::{
    migrate_cursor_mode, read_proxy_settings, CommitPromptLocale, CommitSettings,
    CompactionSettings, CurrencyPricing, CursorMode, DesktopSettingsPatch, ModelTokenPricing,
    PendingUpdate, PricingMode, ProxyMode, ProxySettingsInput, ProxySettingsSecret, Store,
    TokenPrice, TokenPricingSettings, DEFAULT_COMMIT_PROMPT_EN_US, DEFAULT_COMMIT_PROMPT_ZH_CN,
    LEGACY_CURSOR_TAKEOVER_KEY, MODEL_PRICING_SETTINGS_KEY, PROXY_SETTINGS_KEY,
};
use crate::devin::{DevinModelBinding, DevinSettings};
use crate::store::now_ms;

async fn temp_store() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();
    (directory, store)
}

/// 旧版写进 `token_pricing` 的整套全局价，一字不改地放回库里。
async fn insert_legacy_global_row(store: &Store) {
    sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES ('token_pricing', ?, 0)",
        )
        .bind(
            r#"{"mode":"peak_off_peak","cny":{"fixed":{"input_per_million":2.0,"output_per_million":8.0,"cache_read_per_million":0.04,"cache_write_per_million":0.0},"peak":{"input_per_million":2.0,"output_per_million":8.0,"cache_read_per_million":0.04,"cache_write_per_million":0.0},"off_peak":{"input_per_million":1.0,"output_per_million":4.0,"cache_read_per_million":0.02,"cache_write_per_million":0.0}},"usd":{"fixed":{"input_per_million":0.3,"output_per_million":1.2,"cache_read_per_million":0.006,"cache_write_per_million":0.0},"peak":{"input_per_million":0.3,"output_per_million":1.2,"cache_read_per_million":0.006,"cache_write_per_million":0.0},"off_peak":{"input_per_million":0.15,"output_per_million":0.6,"cache_read_per_million":0.003,"cache_write_per_million":0.0}}}"#,
        )
        .execute(store.pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn unchanged_proxy_port_does_not_wait_for_storage_maintenance() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();
    store.set_proxy_port(9061).await.unwrap();
    let maintenance = store.writes.lock().await;
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        store.set_proxy_port(9061),
    )
    .await;
    drop(maintenance);
    assert!(
        result.is_ok(),
        "an unchanged port must not acquire the write lock"
    );
    result.unwrap().unwrap();
    store.set_proxy_port(9062).await.unwrap();
    assert_eq!(store.port_settings().await.unwrap().proxy_port, 9062);
}

/// 服务端有两个写入方各改一个字段（启动写服务端口、接管启动代理写代理端口）：
/// 谁都不许把对方那半行抹掉。值没变的那条路径同样不碰写锁。
#[tokio::test]
async fn partial_port_updates_keep_the_other_field_and_skip_the_write_lock_when_unchanged() {
    let (_directory, store) = temp_store().await;
    store
        .set_port_settings(super::PortSettings {
            proxy_port: 41_000,
            service_port: 56_321,
        })
        .await
        .unwrap();

    // 值没变：不等待写锁（后台回收可能长时间持锁）。
    let maintenance = store.writes.lock().await;
    let unchanged = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        store.set_service_port(56321),
    )
    .await;
    drop(maintenance);
    assert!(
        unchanged.is_ok(),
        "an unchanged port must not take the lock"
    );
    unchanged.unwrap().unwrap();

    store.set_service_port(56_400).await.unwrap();
    store.set_proxy_port(41_100).await.unwrap();
    assert_eq!(
        store.port_settings().await.unwrap(),
        super::PortSettings {
            proxy_port: 41_100,
            service_port: 56_400,
        }
    );
}

/// The `outbound_proxy` row exactly as builds before the `system` -> `default`
/// rename wrote it.
const LEGACY_PROXY_ROW: &str =
    r#"{"mode":"system","address":"","auth_enabled":false,"username":"","password":""}"#;

#[test]
fn commit_prompt_locale_maps_unknown_interface_languages_to_english() {
    assert_eq!(
        serde_json::from_str::<CommitPromptLocale>(r#""zh-CN""#).unwrap(),
        CommitPromptLocale::ZhCn
    );
    assert_eq!(
        serde_json::from_str::<CommitPromptLocale>(r#""en-US""#).unwrap(),
        CommitPromptLocale::EnUs
    );
    assert_eq!(
        serde_json::from_str::<CommitPromptLocale>(r#""ja-JP""#).unwrap(),
        CommitPromptLocale::EnUs
    );
    assert_eq!(
        serde_json::to_string(&CommitPromptLocale::EnUs).unwrap(),
        r#""en-US""#
    );
}

#[test]
fn default_commit_prompt_follows_its_saved_locale() {
    for (prompt_locale, expected) in [
        (CommitPromptLocale::ZhCn, DEFAULT_COMMIT_PROMPT_ZH_CN),
        (CommitPromptLocale::EnUs, DEFAULT_COMMIT_PROMPT_EN_US),
    ] {
        let settings = CommitSettings {
            prompt_locale,
            ..CommitSettings::default()
        };
        assert_eq!(settings.effective_prompt(), expected.trim());
    }
}

#[test]
fn custom_commit_prompt_does_not_change_with_locale() {
    for prompt_locale in [CommitPromptLocale::ZhCn, CommitPromptLocale::EnUs] {
        let settings = CommitSettings {
            prompt: "custom prompt".into(),
            prompt_locale,
            ..CommitSettings::default()
        };
        assert_eq!(settings.effective_prompt(), "custom prompt");
    }
}

#[test]
fn default_proxy_mode_uses_the_default_wire_value() {
    assert_eq!(ProxyMode::default(), ProxyMode::Default);
    assert_eq!(
        serde_json::to_string(&ProxyMode::default()).unwrap(),
        "\"default\""
    );
    assert_eq!(
        serde_json::from_str::<ProxyMode>("\"default\"").unwrap(),
        ProxyMode::Default
    );
    assert!(serde_json::from_str::<ProxyMode>("\"system\"").is_err());
}

/// 直连是一个独立模式：既不读系统代理，也不读填写的地址。
#[test]
fn direct_proxy_mode_round_trips_and_is_not_custom() {
    assert_eq!(
        serde_json::to_string(&ProxyMode::Direct).unwrap(),
        "\"direct\""
    );
    assert_eq!(
        serde_json::from_str::<ProxyMode>("\"direct\"").unwrap(),
        ProxyMode::Direct
    );
    assert!(!ProxyMode::Direct.is_custom());
}

/// 老版本只认 `default` 与 `custom`；库里出现 `direct` 时它读不出来，于是
/// 退回默认值。这条锁住"读不出来也不许报错"：出网客户端全部由这个值构建。
#[test]
fn a_row_written_by_a_newer_build_reads_as_the_default() {
    let settings = read_proxy_settings(r#"{"mode":"direct","address":""}"#);

    assert_eq!(settings.mode, ProxyMode::Default);
}

#[test]
fn an_unreadable_proxy_row_reads_as_no_proxy() {
    assert!(serde_json::from_str::<ProxySettingsSecret>(LEGACY_PROXY_ROW).is_err());

    let settings = read_proxy_settings(LEGACY_PROXY_ROW);
    assert_eq!(settings.mode, ProxyMode::Default);
    assert!(settings.address.is_empty());
    assert!(!settings.auth_enabled);
}

#[tokio::test]
async fn a_database_without_a_choice_stays_in_official_mode() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();

    // A database that has never recorded a choice must not take Cursor over:
    // doing so rewrites the user's Cursor configuration and force-terminates
    // their editor.
    assert_eq!(store.cursor_mode().await.unwrap(), CursorMode::Official);

    store.set_cursor_mode(CursorMode::Mixed).await.unwrap();
    assert_eq!(store.cursor_mode().await.unwrap(), CursorMode::Mixed);

    store.set_cursor_mode(CursorMode::Official).await.unwrap();
    assert_eq!(store.cursor_mode().await.unwrap(), CursorMode::Official);
}

/// 旧版的接管布尔换算成模式：接管过 → 本地（保住"没有官方订阅也能用"），
/// 没接管 → 官方；旧键在任何情况下都不留下。
#[tokio::test]
async fn the_legacy_takeover_boolean_becomes_a_mode_once() {
    for (legacy, expected) in [("true", CursorMode::Local), ("false", CursorMode::Official)] {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", directory.path().join("test.db").display());
        let store = Store::connect(&url).await.unwrap();
        sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, 0)",
        )
        .bind(LEGACY_CURSOR_TAKEOVER_KEY)
        .bind(legacy)
        .execute(store.pool())
        .await
        .unwrap();

        migrate_cursor_mode(store.pool()).await.unwrap();

        assert_eq!(store.cursor_mode().await.unwrap(), expected);
        let leftover: Option<String> =
            sqlx::query_scalar("SELECT value_json FROM service_settings WHERE setting_key = ?")
                .bind(LEGACY_CURSOR_TAKEOVER_KEY)
                .fetch_optional(store.pool())
                .await
                .unwrap();
        assert!(leftover.is_none(), "the legacy key must not survive");
    }
}

/// 已经记过模式的库不被旧键改写，旧键照样清掉。
#[tokio::test]
async fn an_existing_mode_wins_over_the_legacy_boolean() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();
    sqlx::query(
        "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, 0)",
    )
    .bind(LEGACY_CURSOR_TAKEOVER_KEY)
    .bind("true")
    .execute(store.pool())
    .await
    .unwrap();
    store.set_cursor_mode(CursorMode::Official).await.unwrap();

    migrate_cursor_mode(store.pool()).await.unwrap();

    assert_eq!(store.cursor_mode().await.unwrap(), CursorMode::Official);
}

#[tokio::test]
async fn a_proxy_row_from_an_older_build_stays_replaceable() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();
    sqlx::query(
        "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, 0)",
    )
    .bind(PROXY_SETTINGS_KEY)
    .bind(LEGACY_PROXY_ROW)
    .execute(store.pool())
    .await
    .unwrap();

    // Reading must not fail: every outbound client is built from this value.
    assert_eq!(
        store.proxy_settings().await.unwrap().mode,
        ProxyMode::Default
    );

    // And the settings page must be able to overwrite the row that broke it.
    let saved = store
        .set_proxy_settings(ProxySettingsInput {
            mode: ProxyMode::Custom,
            address: "http://127.0.0.1:7890".into(),
            auth_enabled: false,
            username: String::new(),
            password: None,
        })
        .await
        .unwrap();
    assert_eq!(saved.mode, ProxyMode::Custom);
    assert_eq!(saved.address, "http://127.0.0.1:7890");
}

/// 桌面设置按字段保存：一个控件保存自己的字段时，不能撤销另一个控件刚保存的字段。
#[tokio::test]
async fn desktop_settings_update_keeps_the_fields_it_did_not_touch() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();

    store
        .update_desktop_settings(DesktopSettingsPatch {
            silent_start: Some(true),
            ..DesktopSettingsPatch::default()
        })
        .await
        .unwrap();
    let saved = store
        .update_desktop_settings(DesktopSettingsPatch {
            developer_mode: Some(true),
            ..DesktopSettingsPatch::default()
        })
        .await
        .unwrap();

    assert!(saved.silent_start, "the earlier save must survive");
    assert!(saved.developer_mode);
    assert!(
        saved.show_dock_icon,
        "a field nobody wrote keeps its default"
    );
    assert_eq!(store.desktop_settings().await.unwrap(), saved);
}

/// 两次按字段更新同时落库时各自生效：读取-合并-写回必须在同一把写锁内完成，
/// 否则两边都拿着旧的整份设置，后写的那个会把对方的字段撤回去。
#[tokio::test]
async fn concurrent_desktop_settings_updates_keep_every_field() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();

    let (silent_start, show_dock_icon) = tokio::join!(
        store.update_desktop_settings(DesktopSettingsPatch {
            silent_start: Some(true),
            ..DesktopSettingsPatch::default()
        }),
        store.update_desktop_settings(DesktopSettingsPatch {
            show_dock_icon: Some(false),
            ..DesktopSettingsPatch::default()
        }),
    );
    silent_start.unwrap();
    show_dock_icon.unwrap();

    let settings = store.desktop_settings().await.unwrap();
    assert!(settings.silent_start);
    assert!(!settings.show_dock_icon);
}

/// 三个开关与界面偏好一起按字段保存：改一个不能撤销别的，PUT 回显整份设置。
///
/// 前端只有看到回显才删旧的 localStorage 键（"回显确认"），所以 PUT 的返回值必须是
/// 保存后的完整设置——这条用例同时钉住"部分更新保留其余字段"和"回显完整"。
#[tokio::test]
async fn desktop_settings_round_trip_and_partial_updates_keep_the_other_fields() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();

    let saved = store
        .update_desktop_settings(patch(
            r#"{"theme":"midnight","locale":"zh-CN","tutorial_read":true,
                    "pending_update":{"from":"1.0.0","to":"1.1.0"},
                    "devin_host_path":"/usr/local/bin/devin",
                    "devin_install_root":"/opt/Devin",
                    "disclosure":{"models":true,"storage":false}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(saved.theme.as_deref(), Some("midnight"));
    assert_eq!(saved.locale.as_deref(), Some("zh-CN"));
    assert_eq!(saved.tutorial_read, Some(true));
    assert_eq!(
        saved.pending_update,
        Some(PendingUpdate {
            from: "1.0.0".into(),
            to: "1.1.0".into()
        })
    );
    assert_eq!(
        saved.devin_host_path.as_deref(),
        Some("/usr/local/bin/devin")
    );
    assert_eq!(saved.devin_install_root.as_deref(), Some("/opt/Devin"));
    assert_eq!(saved.disclosure.get("models"), Some(&true));
    assert_eq!(
        store.desktop_settings().await.unwrap(),
        saved,
        "回显 = 落库的样子"
    );

    // 只改一个开关与一个偏好字段：其余原样保留。
    let later = store
        .update_desktop_settings(patch(r#"{"silent_start":true,"locale":"en-US"}"#))
        .await
        .unwrap();
    assert!(later.silent_start);
    assert_eq!(later.locale.as_deref(), Some("en-US"));
    assert_eq!(later.theme.as_deref(), Some("midnight"), "别的字段保持原值");
    assert_eq!(later.tutorial_read, Some(true));
    assert_eq!(later.devin_install_root.as_deref(), Some("/opt/Devin"));
    assert_eq!(later.disclosure.get("storage"), Some(&false));
    assert_eq!(store.desktop_settings().await.unwrap(), later);
}

/// `null` = 清空回默认，缺席 = 不改；两者必须分得开。
///
/// serde 默认把 `null` 和缺席都解成 `None`，那样前端点"清空"会被当成"不改"——正好相反。
#[tokio::test]
async fn a_null_field_is_cleared_and_an_absent_field_is_left_alone() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();
    store
        .update_desktop_settings(patch(
            r#"{"theme":"midnight","locale":"zh-CN","tutorial_read":true}"#,
        ))
        .await
        .unwrap();

    let cleared = store
        .update_desktop_settings(patch(r#"{"theme":null}"#))
        .await
        .unwrap();

    assert_eq!(cleared.theme, None, "null 清空");
    assert_eq!(cleared.locale.as_deref(), Some("zh-CN"), "缺席不改");
    assert_eq!(cleared.tutorial_read, Some(true), "缺席不改");
}

/// 两个 Devin 路径都能被显式清空：页面清掉失效的自定义位置后，下次启动不该再拿旧值
/// 去探测。这里同时钉住 `devin_install_root` 的落库与清除形状。
#[tokio::test]
async fn devin_host_paths_can_be_cleared_with_null() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();
    store
        .update_desktop_settings(patch(
            r#"{"devin_host_path":"D:\\Devin\\User\\settings.json",
                    "devin_install_root":"D:\\Devin"}"#,
        ))
        .await
        .unwrap();

    let cleared = store
        .update_desktop_settings(patch(
            r#"{"devin_host_path":null,"devin_install_root":null}"#,
        ))
        .await
        .unwrap();

    assert_eq!(cleared.devin_host_path, None);
    assert_eq!(cleared.devin_install_root, None);
}

/// `disclosure` 按卡片键合并：两个卡片先后折叠不会互相撤销；`null` 才是清空整份。
#[tokio::test]
async fn disclosure_merges_per_card_key_and_null_clears_everything() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();

    store
        .update_desktop_settings(patch(r#"{"disclosure":{"models":true}}"#))
        .await
        .unwrap();
    let merged = store
        .update_desktop_settings(patch(r#"{"disclosure":{"storage":false}}"#))
        .await
        .unwrap();
    assert_eq!(
        merged.disclosure.get("models"),
        Some(&true),
        "合并保留别的卡片"
    );
    assert_eq!(merged.disclosure.get("storage"), Some(&false));

    let overridden = store
        .update_desktop_settings(patch(r#"{"disclosure":{"models":false}}"#))
        .await
        .unwrap();
    assert_eq!(overridden.disclosure.get("models"), Some(&false));
    assert_eq!(overridden.disclosure.get("storage"), Some(&false));

    let cleared = store
        .update_desktop_settings(patch(r#"{"disclosure":null}"#))
        .await
        .unwrap();
    assert!(cleared.disclosure.is_empty(), "null 清空整份折叠状态");
}

/// 旧库（这一版之前写下的设置 JSON）读出来必须是"还没有值"，而不是解析失败。
///
/// 设置是一行 JSON（`service_settings.value_json`），新增字段靠 `#[serde(default)]` 补齐：
/// 旧行里没有 `theme` 等键 → `None`（前端据此判定"服务端没有值、用本地默认"），
/// `disclosure` → 空表。所以这一版不需要迁移，但必须有用例证明旧行仍可读。
#[tokio::test]
async fn an_old_settings_row_reads_absent_ui_preferences() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();

    // 这一版之前落库的形状：只有三个开关。
    sqlx::query(
        "INSERT INTO service_settings(setting_key, value_json, updated_at_ms)
             VALUES (?, ?, ?)",
    )
    .bind("desktop_lifecycle")
    .bind(r#"{"silent_start":true,"show_dock_icon":false,"developer_mode":true}"#)
    .bind(now_ms())
    .execute(store.pool())
    .await
    .unwrap();

    let settings = store.desktop_settings().await.unwrap();
    assert!(settings.silent_start && settings.developer_mode);
    assert!(!settings.show_dock_icon);
    assert_eq!(settings.theme, None);
    assert_eq!(settings.locale, None);
    assert_eq!(settings.tutorial_read, None);
    assert_eq!(settings.pending_update, None);
    assert_eq!(settings.devin_host_path, None);
    assert_eq!(settings.devin_install_root, None);
    assert!(settings.disclosure.is_empty());

    // 旧行被改一个开关后写回去，界面偏好仍然是"没有值"。
    let saved = store
        .update_desktop_settings(patch(r#"{"developer_mode":false}"#))
        .await
        .unwrap();
    assert!(!saved.developer_mode && saved.silent_start);
    assert_eq!(saved.theme, None);
    assert!(saved.disclosure.is_empty());
}

/// 两个控件同时保存时各自生效：界面偏好也在同一把写锁里读-改-写。
#[tokio::test]
async fn concurrent_ui_preference_updates_keep_every_field() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();

    let (theme, locale, disclosure) = tokio::join!(
        store.update_desktop_settings(patch(r#"{"theme":"midnight"}"#)),
        store.update_desktop_settings(patch(r#"{"locale":"en-US"}"#)),
        store.update_desktop_settings(patch(r#"{"disclosure":{"models":true}}"#)),
    );
    theme.unwrap();
    locale.unwrap();
    disclosure.unwrap();

    let settings = store.desktop_settings().await.unwrap();
    assert_eq!(settings.theme.as_deref(), Some("midnight"));
    assert_eq!(settings.locale.as_deref(), Some("en-US"));
    assert_eq!(settings.disclosure.get("models"), Some(&true));
}

/// 把一段 PUT 正文解成修订：用例走真实的 serde 形状（缺席 / `null` / 有值）。
fn patch(json: &str) -> DesktopSettingsPatch {
    serde_json::from_str(json).unwrap()
}

/// 一条只写了人民币固定单价的规则；美元没有配置，是未定价而不是 0 元。
fn fixed_rule(input_per_million: f64) -> ModelTokenPricing {
    ModelTokenPricing {
        mode: PricingMode::Fixed,
        cny: Some(CurrencyPricing {
            fixed: TokenPrice {
                input_per_million,
                output_per_million: input_per_million * 4.0,
                cache_read_per_million: 0.0,
                cache_write_per_million: 0.0,
            },
            ..CurrencyPricing::zero()
        }),
        ..ModelTokenPricing::default()
    }
}

/// 测试里的规则只配了人民币，取价时直接拿这一套。
fn cny(pricing: &ModelTokenPricing) -> CurrencyPricing {
    pricing.cny.expect("cny rule")
}

/// 从没存过规则的库不给任何模型定价：不存在全局价，也不预填厂商价。
#[tokio::test]
async fn a_database_without_rules_prices_no_model() {
    let (_directory, store) = temp_store().await;

    assert!(store.pricing_settings().await.unwrap().models.is_empty());
}

/// 保存一个模型的价格不会动到别的模型，读回逐字段一致。
#[tokio::test]
async fn saving_a_rule_for_a_second_model_keeps_the_first() {
    let (_directory, store) = temp_store().await;

    store
        .set_model_pricing("aaaa", fixed_rule(2.0))
        .await
        .unwrap();
    let saved = store
        .set_model_pricing("bbbb", fixed_rule(4.0))
        .await
        .unwrap();

    assert_eq!(saved.models.len(), 2);
    let first = &saved.models["aaaa"];
    assert_eq!(first.mode, PricingMode::Fixed);
    assert_eq!(cny(first).fixed.input_per_million, 2.0);
    assert_eq!(cny(first).fixed.output_per_million, 8.0);
    assert_eq!(cny(first).peak.input_per_million, 0.0);
    assert!(first.usd.is_none(), "没配的币种保持未定价");
    assert_eq!(cny(&saved.models["bbbb"]).fixed.input_per_million, 4.0);
    assert_eq!(store.pricing_settings().await.unwrap(), saved);
}

/// 两个模型各自保存时都要留下。用一条 `BEGIN IMMEDIATE` 事务把 SQLite 的写锁
/// 攥在手里，逼出"两个保存都读到旧表、又都卡在写入口"的窗口：读回-合并-写回
/// 不在同一把写锁内完成时，后写的那个会把对方刚存的规则撤掉。
#[tokio::test]
async fn concurrent_saves_of_two_models_both_survive() {
    let (_directory, store) = temp_store().await;
    let blocker = store.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();

    let first = tokio::spawn({
        let store = store.clone();
        async move { store.set_model_pricing("aaaa", fixed_rule(2.0)).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let second = tokio::spawn({
        let store = store.clone();
        async move { store.set_model_pricing("bbbb", fixed_rule(4.0)).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    blocker.rollback().await.unwrap();

    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();

    let saved = store.pricing_settings().await.unwrap();
    assert_eq!(saved.models.len(), 2);
    assert_eq!(cny(&saved.models["aaaa"]).fixed.input_per_million, 2.0);
    assert_eq!(cny(&saved.models["bbbb"]).fixed.input_per_million, 4.0);
}

/// 删掉一个模型的价格，别的模型的价格还在。
#[tokio::test]
async fn deleting_one_models_rule_keeps_the_other() {
    let (_directory, store) = temp_store().await;
    store
        .set_model_pricing("aaaa", fixed_rule(2.0))
        .await
        .unwrap();
    store
        .set_model_pricing("bbbb", fixed_rule(4.0))
        .await
        .unwrap();

    let remaining = store.remove_model_pricing("aaaa").await.unwrap();

    assert!(!remaining.models.contains_key("aaaa"));
    assert_eq!(cny(&remaining.models["bbbb"]).fixed.input_per_million, 4.0);
    assert_eq!(store.pricing_settings().await.unwrap(), remaining);
}

/// 删除一条不存在的规则是空操作：不报错，也不改动已有规则。
#[tokio::test]
async fn removing_a_rule_that_does_not_exist_changes_nothing() {
    let (_directory, store) = temp_store().await;
    store
        .set_model_pricing("aaaa", fixed_rule(2.0))
        .await
        .unwrap();

    let remaining = store.remove_model_pricing("cccc").await.unwrap();

    assert_eq!(remaining.models.len(), 1);
    assert_eq!(cny(&remaining.models["aaaa"]).fixed.input_per_million, 2.0);
    assert_eq!(store.pricing_settings().await.unwrap(), remaining);
}

/// 负价与非有限价一律拒绝，且拒绝时库里不留任何痕迹。
#[tokio::test]
async fn negative_and_non_finite_rates_are_rejected() {
    let (_directory, store) = temp_store().await;
    let rejected = [
        ModelTokenPricing {
            cny: Some(CurrencyPricing {
                fixed: TokenPrice {
                    input_per_million: -0.01,
                    ..TokenPrice::zero()
                },
                ..CurrencyPricing::zero()
            }),
            ..ModelTokenPricing::default()
        },
        ModelTokenPricing {
            cny: Some(CurrencyPricing {
                peak: TokenPrice {
                    output_per_million: f64::NAN,
                    ..TokenPrice::zero()
                },
                ..CurrencyPricing::zero()
            }),
            ..ModelTokenPricing::default()
        },
        ModelTokenPricing {
            usd: Some(CurrencyPricing {
                off_peak: TokenPrice {
                    cache_write_per_million: f64::INFINITY,
                    ..TokenPrice::zero()
                },
                ..CurrencyPricing::zero()
            }),
            ..ModelTokenPricing::default()
        },
    ];

    for pricing in rejected {
        let error = store.set_model_pricing("aaaa", pricing).await.unwrap_err();
        assert!(
            matches!(error, crate::Error::Config(_)),
            "expected a configuration error, got {error}"
        );
    }
    assert!(store.pricing_settings().await.unwrap().models.is_empty());
}

/// 价格规则挂在稳定模型标识上，标识必须干净：空白或空的键一律拒绝
/// （静默 trim 会造出第二个看不见的键）。
#[tokio::test]
async fn a_blank_model_hash_is_rejected() {
    let (_directory, store) = temp_store().await;

    for blank in ["", "   ", "aaaa\nbbbb"] {
        assert!(
            store
                .set_model_pricing(blank, fixed_rule(1.0))
                .await
                .is_err(),
            "{blank:?} must be rejected"
        );
        assert!(store.remove_model_pricing(blank).await.is_err());
    }
    assert!(store.pricing_settings().await.unwrap().models.is_empty());

    // 插件模型标识带 `/` 与 `:`，是正常键。
    let plugin_model = "plugin:dev.example/codex/gpt-test";
    store
        .set_model_pricing(plugin_model, fixed_rule(1.0))
        .await
        .unwrap();
    assert!(store
        .pricing_settings()
        .await
        .unwrap()
        .models
        .contains_key(plugin_model));
}

/// 旧版的整套全局价绝不自动套到任何模型上——包括用户后来保存新规则时。
#[tokio::test]
async fn the_legacy_global_row_is_never_applied_to_a_model() {
    let (_directory, store) = temp_store().await;
    insert_legacy_global_row(&store).await;

    assert!(store.pricing_settings().await.unwrap().models.is_empty());

    let saved = store
        .set_model_pricing("aaaa", fixed_rule(2.0))
        .await
        .unwrap();
    assert_eq!(saved.models.len(), 1);
    assert_eq!(cny(&saved.models["aaaa"]).fixed.input_per_million, 2.0);
    assert_eq!(cny(&saved.models["aaaa"]).fixed.output_per_million, 8.0);
}

/// 用户成功保存第一条按模型规则后，旧版全局价那行被清掉：它的语义已经被取代，
/// 留着只会让人以为还有第二处真相。
#[tokio::test]
async fn an_explicit_save_retires_the_legacy_global_row() {
    let (_directory, store) = temp_store().await;
    insert_legacy_global_row(&store).await;

    store
        .set_model_pricing("aaaa", fixed_rule(2.0))
        .await
        .unwrap();

    let leftover: Option<String> = sqlx::query_scalar(
        "SELECT value_json FROM service_settings WHERE setting_key = 'token_pricing'",
    )
    .fetch_optional(store.pool())
    .await
    .unwrap();
    assert!(leftover.is_none(), "the legacy global row must not survive");
    assert!(store
        .pricing_settings()
        .await
        .unwrap()
        .models
        .contains_key("aaaa"));
}

/// 新建模型的初始草稿是固定单价、两个币种都未定价；代码不预填任何厂商的官方价，
/// 未定价就是未定价。
#[test]
fn a_new_model_draft_is_fixed_and_unpriced() {
    let draft = ModelTokenPricing::default();

    assert_eq!(draft.mode, PricingMode::Fixed);
    assert!(draft.cny.is_none());
    assert!(draft.usd.is_none());
}

/// 没写全的规则体不会继承任何厂商价：写了的币种照收，缺的位补 0，
/// 没写的币种保持未定价——它不能被看成"价格为零"。
#[test]
fn a_partial_rule_body_leaves_the_other_currency_unpriced() {
    let draft: ModelTokenPricing = serde_json::from_str("{}").unwrap();
    assert_eq!(draft, ModelTokenPricing::default());

    let partial: ModelTokenPricing = serde_json::from_str(
        r#"{"mode":"peak_off_peak","cny":{"peak":{"input_per_million":1.5}}}"#,
    )
    .unwrap();
    assert_eq!(partial.mode, PricingMode::PeakOffPeak);
    let cny = partial.cny.expect("cny rule");
    assert_eq!(cny.peak.input_per_million, 1.5);
    assert_eq!(cny.peak.output_per_million, 0.0);
    assert_eq!(cny.fixed.input_per_million, 0.0);
    assert!(partial.usd.is_none());
}

/// 价格表读不出来时不能变成"改也改不了"：读成空表，下一次保存覆盖坏行。
#[tokio::test]
async fn an_unreadable_pricing_row_is_replaced_instead_of_blocking_saves() {
    let (_directory, store) = temp_store().await;
    sqlx::query(
        "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?)",
    )
    .bind(MODEL_PRICING_SETTINGS_KEY)
    .bind(r#"{"models":{"aaaa":{"mode":"hourly","cny":null}}}"#)
    .bind(now_ms())
    .execute(store.pool())
    .await
    .unwrap();

    assert!(store.pricing_settings().await.unwrap().models.is_empty());

    let saved = store
        .set_model_pricing("aaaa", fixed_rule(2.0))
        .await
        .unwrap();
    assert_eq!(saved.models.len(), 1);
    assert_eq!(cny(&saved.models["aaaa"]).fixed.input_per_million, 2.0);
    assert_eq!(store.pricing_settings().await.unwrap(), saved);
}

/// 旧版全局价那行的形状永远不会变成按模型规则：前端收到的仍是空表。
#[test]
fn a_global_row_shape_never_becomes_a_model_rule() {
    let settings: TokenPricingSettings = serde_json::from_str(
            r#"{"mode":"fixed","cny":{"fixed":{"input_per_million":5.0,"output_per_million":25.0,"cache_read_per_million":0.5,"cache_write_per_million":6.25},"peak":{},"off_peak":{}},"usd":{}}"#,
        )
        .unwrap();

    assert!(settings.models.is_empty());
}

/// 与前端共享的线上形状：一个 `models` 映射，键是稳定模型标识，值带
/// `mode` 与各币种（未配置的币种是 `null`）完整的单价。
#[test]
fn the_wire_shape_is_a_map_of_model_rules() {
    let settings = TokenPricingSettings {
        models: BTreeMap::from([(
            "aaaa".to_owned(),
            ModelTokenPricing {
                mode: PricingMode::Fixed,
                cny: Some(CurrencyPricing {
                    fixed: TokenPrice {
                        input_per_million: 1.0,
                        output_per_million: 8.0,
                        cache_read_per_million: 0.04,
                        cache_write_per_million: 0.0,
                    },
                    ..CurrencyPricing::zero()
                }),
                ..ModelTokenPricing::default()
            },
        )]),
    };
    let wire = serde_json::json!({
        "models": {
            "aaaa": {
                "mode": "fixed",
                "cny": {
                    "fixed": {
                        "input_per_million": 1.0,
                        "output_per_million": 8.0,
                        "cache_read_per_million": 0.04,
                        "cache_write_per_million": 0.0
                    },
                    "peak": {
                        "input_per_million": 0.0,
                        "output_per_million": 0.0,
                        "cache_read_per_million": 0.0,
                        "cache_write_per_million": 0.0
                    },
                    "off_peak": {
                        "input_per_million": 0.0,
                        "output_per_million": 0.0,
                        "cache_read_per_million": 0.0,
                        "cache_write_per_million": 0.0
                    }
                },
                "usd": null
            }
        }
    });

    assert_eq!(serde_json::to_value(&settings).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<TokenPricingSettings>(wire).unwrap(),
        settings
    );
}

#[test]
fn devin_defaults_are_disabled_and_use_isolated_ports() {
    let settings = DevinSettings::default();

    assert!(!settings.enabled);
    assert_eq!(settings.api_port, 43_110);
    assert_eq!(settings.inference_port, 43_111);
    assert_eq!(settings.local_api_port, 43_112);
    assert!(settings.bindings.is_empty());
    assert_eq!(settings.default_model_uid, None);
}

#[test]
fn devin_settings_reject_duplicate_model_uids() {
    let settings = DevinSettings {
        bindings: vec![
            DevinModelBinding::new("model-a", "hash-a"),
            DevinModelBinding::new("model-a", "hash-b"),
        ],
        ..DevinSettings::default()
    };

    let error = settings.validate().unwrap_err().to_string();
    assert!(error.contains("duplicate Devin model UID"));
}

/// 连续两次读取只解析一次：版本没变就直接用缓存（Devin 网关每个请求读一次设置）。
///
/// 逆变检查：把版本号判据去掉（每次读都重新解析）→ 这条用例必红（解析次数变成 2）。
#[tokio::test]
async fn reading_devin_settings_twice_parses_once() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();
    store
        .set_devin_settings(DevinSettings {
            enabled: true,
            bindings: vec![DevinModelBinding::new("deven-model", "0123abcd")],
            ..DevinSettings::default()
        })
        .await
        .unwrap();
    let parses_before = store
        .devin_settings_cache
        .parses
        .load(std::sync::atomic::Ordering::Relaxed);

    let first = store.devin_settings().await.unwrap();
    let second = store.devin_settings().await.unwrap();
    assert_eq!(first, second);
    assert!(first.enabled);
    assert_eq!(
        store
            .devin_settings_cache
            .parses
            .load(std::sync::atomic::Ordering::Relaxed)
            - parses_before,
        1,
        "第二次读取必须命中缓存，不能再解析一遍"
    );

    // 写一次设置 → 版本号变 → 下一次读取必须重新解析，缓存不会给出旧设置。
    let changed = DevinSettings {
        enabled: false,
        bindings: vec![DevinModelBinding::new("deven-model", "0123abcd")],
        ..first.clone()
    };
    store.set_devin_settings(changed.clone()).await.unwrap();
    let reloaded = store.devin_settings().await.unwrap();
    assert_eq!(reloaded, changed, "写后必须读到新设置");
    assert!(!reloaded.enabled);
    assert_eq!(
        store
            .devin_settings_cache
            .parses
            .load(std::sync::atomic::Ordering::Relaxed)
            - parses_before,
        2,
        "设置变了要重新解析一次"
    );
}

#[tokio::test]
async fn devin_settings_persist_without_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();
    let settings = DevinSettings {
        enabled: true,
        bindings: vec![DevinModelBinding::new("deven-model", "0123abcd")],
        ..DevinSettings::default()
    };

    assert_eq!(
        store.set_devin_settings(settings.clone()).await.unwrap(),
        settings
    );
    assert_eq!(store.devin_settings().await.unwrap(), settings);

    let raw = sqlx::query_scalar::<_, String>(
        "SELECT value_json FROM service_settings WHERE setting_key = 'devin_router'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert!(!raw.contains("api_key"));
}

/// 旧版本的 Devin 设置行在读取时必须完整可用：绑定保持启用，路由字段取默认值，
/// 原主哈希仍然是生效模型。保存回读不得改变这三点。
#[tokio::test]
async fn legacy_devin_row_keeps_its_primary_hash_across_load_and_save() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();
    let legacy = r#"{"enabled":true,"auth_token":"","api_port":43110,"inference_port":43111,"local_api_port":43112,"bindings":[{"model_uid":"deven-model","model_hash":"0123abcd","display_name":"Legacy","enabled":true}]}"#;
    sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES ('devin_router', ?, 0)",
        )
        .bind(legacy)
        .execute(store.pool())
        .await
        .unwrap();

    let loaded = store.devin_settings().await.unwrap();
    assert!(loaded.enabled);
    let binding = &loaded.bindings[0];
    assert_eq!(binding.model_uid, "deven-model");
    assert_eq!(binding.model_hash, "0123abcd");
    assert!(binding.enabled);
    assert!(binding.routes.is_empty());
    assert_eq!(binding.active_route_id, None);
    assert_eq!(binding.effective_model_hash(), "0123abcd");

    // Saving through the store must not silently migrate the binding away
    // from its legacy primary hash.
    store.set_devin_settings(loaded.clone()).await.unwrap();
    let reloaded = store.devin_settings().await.unwrap();
    assert_eq!(reloaded.bindings[0].model_hash, "0123abcd");
    assert!(reloaded.bindings[0].routes.is_empty());
    assert_eq!(reloaded.bindings[0].effective_model_hash(), "0123abcd");
}

#[tokio::test]
async fn compaction_settings_default_round_trip_and_validation() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();

    let defaults = store.compaction_settings().await.unwrap();
    assert!(defaults.auto);
    assert!(defaults.summary_model_hash.is_empty());
    assert_eq!(defaults.summary_output_tokens, None);

    let saved = store
        .set_compaction_settings(CompactionSettings {
            auto: false,
            summary_model_hash: " 0123abcd ".into(),
            summary_output_tokens: Some(2_048),
            reserve_percent: Some(15),
            retention_percent: Some(12),
            max_retained_tokens: Some(32_768),
        })
        .await
        .unwrap();
    assert!(!saved.auto);
    assert_eq!(saved.summary_model_hash, "0123abcd");
    assert_eq!(saved.summary_output_tokens, Some(2_048));
    assert_eq!(saved.reserve_percent, Some(15));
    assert_eq!(saved.retention_percent, Some(12));
    assert_eq!(saved.max_retained_tokens, Some(32_768));
    assert_eq!(store.compaction_settings().await.unwrap(), saved);

    let invalid = store
        .set_compaction_settings(CompactionSettings {
            summary_output_tokens: Some(0),
            ..CompactionSettings::default()
        })
        .await;
    assert!(
        matches!(invalid, Err(crate::Error::Config(_))),
        "a zero summary cap must be rejected"
    );
    assert_eq!(
        store.compaction_settings().await.unwrap(),
        saved,
        "a rejected save must not overwrite the stored policy"
    );
}

/// Writes the compaction row without going through the validating setter:
/// simulates an external edit or a row left by a build with older rules.
async fn write_raw_compaction_row(store: &Store, value_json: &str) {
    sqlx::query(
            "INSERT INTO service_settings(setting_key, value_json, updated_at_ms) VALUES (?, ?, ?) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(super::COMPACTION_SETTINGS_KEY)
        .bind(value_json)
        .bind(0_i64)
        .execute(&store.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn compaction_settings_tolerates_a_corrupt_or_invalid_row() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();

    // A good row still reads back as itself; the fallback must not erase it.
    let saved = store
        .set_compaction_settings(CompactionSettings {
            auto: false,
            summary_model_hash: "0123abcd".into(),
            summary_output_tokens: Some(2_048),
            reserve_percent: Some(15),
            retention_percent: Some(12),
            max_retained_tokens: Some(32_768),
        })
        .await
        .unwrap();
    assert_eq!(store.compaction_settings().await.unwrap(), saved);

    // Unparseable JSON, a field out of range, a failing cross-field rule and
    // an out-of-bounds cap all read as the shipped defaults instead of
    // failing the settings page (and, through it, every run).
    for bad_row in [
        "not json at all",
        r#"{"auto":false,"reserve_percent":0}"#,
        r#"{"reserve_percent":50,"retention_percent":50}"#,
        r#"{"summary_output_tokens":32769}"#,
    ] {
        write_raw_compaction_row(&store, bad_row).await;
        assert_eq!(
            store.compaction_settings().await.unwrap(),
            CompactionSettings::default(),
            "row {bad_row} must fall back to the shipped defaults"
        );
    }

    // The next successful save repairs the row and is readable again.
    assert_eq!(
        store.set_compaction_settings(saved.clone()).await.unwrap(),
        saved
    );
    assert_eq!(store.compaction_settings().await.unwrap(), saved);
}

#[tokio::test]
async fn compaction_settings_bounds_are_enforced_on_write() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("test.db").display());
    let store = Store::connect(&url).await.unwrap();

    // Boundaries that must be accepted.
    let accepted = [
        CompactionSettings {
            reserve_percent: Some(50),
            retention_percent: Some(30),
            ..CompactionSettings::default()
        },
        CompactionSettings {
            reserve_percent: None,
            retention_percent: Some(50),
            ..CompactionSettings::default()
        },
        CompactionSettings {
            summary_output_tokens: Some(32_768),
            max_retained_tokens: Some(1_000_000),
            ..CompactionSettings::default()
        },
        CompactionSettings::default(),
    ];
    for settings in accepted {
        assert!(
            store
                .set_compaction_settings(settings.clone())
                .await
                .is_ok(),
            "{settings:?} must be accepted"
        );
    }

    // The cross-field rule reads unset fields as their shipped defaults:
    // 50 + 31 = 81 is rejected, and the error names the effective values.
    let over_sum = store
        .set_compaction_settings(CompactionSettings {
            reserve_percent: Some(50),
            retention_percent: Some(31),
            ..CompactionSettings::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(&over_sum, crate::Error::Config(_)));
    let message = over_sum.to_string();
    assert!(message.contains("at most 80"), "{message}");
    assert!(message.contains("reserve 50%"), "{message}");
    assert!(message.contains("retention 31%"), "{message}");

    let rejected = [
        CompactionSettings {
            reserve_percent: Some(40),
            retention_percent: Some(50),
            ..CompactionSettings::default()
        },
        CompactionSettings {
            summary_output_tokens: Some(32_769),
            ..CompactionSettings::default()
        },
        CompactionSettings {
            max_retained_tokens: Some(1_000_001),
            ..CompactionSettings::default()
        },
    ];
    for settings in rejected {
        assert!(
            matches!(
                store.set_compaction_settings(settings.clone()).await,
                Err(crate::Error::Config(_))
            ),
            "{settings:?} must be rejected"
        );
    }
}
