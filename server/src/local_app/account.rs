//! Integrates local Cursor account state.
use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::json;
use sqlx::{Connection, Row, SqliteConnection};

use crate::{Error, Result};

const EMAIL: &str = "cursor@ai.com";
const SIGN_UP_TYPE: &str = "Google";
const SUBJECT: &str = "cursor-local-user";
const MEMBERSHIP_TYPE: &str = "ultra";
const SUBSCRIPTION_STATUS: &str = "active";

/// 注入时写进 Cursor 账号状态的键。`injected_values` 是它们唯一的值来源，清除也
/// 按这份清单走：改注入内容时清除自动跟上，不会漏掉某个键、在客户端里留下半个
/// 本地账号。
const INJECTED_KEYS: [&str; 7] = [
    "cursorAuth/accessToken",
    "cursorAuth/refreshToken",
    "cursorAuth/cachedEmail",
    "cursorAuth/cachedSignUpType",
    "cursorAuth/stripeMembershipAuthId",
    "cursorAuth/stripeMembershipType",
    "cursorAuth/stripeSubscriptionStatus",
];

/// 客户端从我们 mock 的账号接口里缓存下来的展示资料。它不是我们写的，但它是本地
/// 账号的产物：清账号时必须一起走，否则账号菜单还会显示 "Cursor Local"。
const DERIVED_ACCOUNT_KEY: &str = "cursorAuth/cachedScopedProfile";

/// 客户端当前用哪种账号在跟我们说话。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    /// 客户端里没有任何 token：还没登录过。
    Missing,
    /// 我们注入的本地账号（客户端没有官方订阅时的兜底）。
    Local,
    /// 用户自己的 Cursor 账号。
    Official,
}

/// 账号状态 + 邮箱，给状态接口和前置条件判断用。
#[derive(Clone, Debug, serde::Serialize)]
pub struct CursorAccount {
    pub state: AccountState,
    pub email: Option<String>,
}

pub async fn inject_if_missing() -> Result<()> {
    inject_if_missing_at(&state_db_path()?).await
}

/// 读出客户端当前的账号状态。
///
/// 读不出来（没有这个文件、表还不存在）按"没有账号"处理：这个函数的用途是给用户
/// 看状态和判断混合模式的前置条件，不是鉴权。
pub async fn account_state() -> CursorAccount {
    let path = match state_db_path() {
        Ok(path) => path,
        Err(error) => return unreadable_account(&error),
    };
    match read_account_state_at(&path).await {
        Ok(account) => account,
        Err(error) => unreadable_account(&error),
    }
}

fn unreadable_account(error: &Error) -> CursorAccount {
    tracing::debug!(
        error = %crate::diagnostics::error_chain(error),
        "cannot read the Cursor account state"
    );
    CursorAccount {
        state: AccountState::Missing,
        email: None,
    }
}

/// 清掉我们注入过的本地账号，让客户端回到"未登录"去登录自己的账号。
///
/// 只在 `cursorAuth/accessToken` 仍等于我们写的那串时动手：值换成别的，说明用户
/// 真的登录过，那些键归他，我们一根手指都不碰。返回是否真的清了东西。
pub async fn clear_injected_account() -> Result<bool> {
    clear_injected_account_at(&state_db_path()?).await
}

async fn read_account_state_at(path: &Path) -> Result<CursorAccount> {
    if !path.exists() {
        return Ok(CursorAccount {
            state: AccountState::Missing,
            email: None,
        });
    }
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(path);
    let mut connection = SqliteConnection::connect_with(&options).await?;
    let stored = read_account_value(&mut connection, "cursorAuth/accessToken").await?;
    let Some(token) = stored.filter(|value| !value.trim().is_empty()) else {
        return Ok(CursorAccount {
            state: AccountState::Missing,
            email: None,
        });
    };
    let email = read_account_value(&mut connection, "cursorAuth/cachedEmail")
        .await?
        .filter(|value| !value.trim().is_empty());
    let state = if is_local_cursor_token(&token) {
        AccountState::Local
    } else {
        AccountState::Official
    };
    Ok(CursorAccount { state, email })
}

async fn clear_injected_account_at(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(path);
    let mut connection = SqliteConnection::connect_with(&options).await?;
    let stored = match read_account_value(&mut connection, "cursorAuth/accessToken").await {
        Ok(stored) => stored,
        // 表还不存在：没有任何本地账号可清。
        Err(_) => return Ok(false),
    };
    let Some(token) = stored else {
        return Ok(false);
    };
    if !is_local_cursor_token(&token) {
        return Ok(false);
    }
    let mut transaction = connection.begin().await?;
    for key in INJECTED_KEYS.into_iter().chain([DERIVED_ACCOUNT_KEY]) {
        sqlx::query("DELETE FROM ItemTable WHERE key = ?")
            .bind(key)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    tracing::info!("cleared the injected local Cursor account");
    Ok(true)
}

async fn read_account_value(
    connection: &mut SqliteConnection,
    key: &str,
) -> Result<Option<String>> {
    let row = sqlx::query("SELECT CAST(value AS TEXT) AS value FROM ItemTable WHERE key = ?")
        .bind(key)
        .fetch_optional(&mut *connection)
        .await?;
    Ok(row.and_then(|row| row.try_get::<String, _>("value").ok()))
}

fn injected_values(token: &str) -> [(&'static str, String); 7] {
    [
        (INJECTED_KEYS[0], token.to_owned()),
        (INJECTED_KEYS[1], token.to_owned()),
        (INJECTED_KEYS[2], EMAIL.to_owned()),
        (INJECTED_KEYS[3], SIGN_UP_TYPE.to_owned()),
        (INJECTED_KEYS[4], SUBJECT.to_owned()),
        (INJECTED_KEYS[5], MEMBERSHIP_TYPE.to_owned()),
        (INJECTED_KEYS[6], SUBSCRIPTION_STATUS.to_owned()),
    ]
}

fn state_db_path() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| Error::Config("cannot resolve user home directory".into()))?;
    match std::env::consts::OS {
        "macos" => {
            Ok(home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"))
        }
        "windows" => Ok(std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("Cursor/User/globalStorage/state.vscdb")),
        "linux" => Ok(std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("Cursor/User/globalStorage/state.vscdb")),
        platform => Err(Error::Config(format!(
            "Cursor account injection is unsupported on {platform}"
        ))),
    }
}

async fn inject_if_missing_at(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let mut connection = SqliteConnection::connect_with(&options).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
    )
    .execute(&mut connection)
    .await?;

    let token = local_token()?;
    let account = sqlx::query("SELECT CAST(value AS TEXT) AS value FROM ItemTable WHERE key = ?")
        .bind("cursorAuth/accessToken")
        .fetch_optional(&mut connection)
        .await?;
    if account.is_some_and(|row| {
        row.try_get::<String, _>("value")
            .is_ok_and(|value| !value.trim().is_empty() && value != token)
    }) {
        return Ok(());
    }

    let values = injected_values(&token);
    let mut transaction = connection.begin().await?;
    for (key, value) in values {
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(key)
            .bind(value)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    tracing::info!(
        email = EMAIL,
        subject = SUBJECT,
        "injected local Cursor account"
    );
    Ok(())
}

pub(crate) fn is_local_cursor_authorization(authorization: &str) -> bool {
    authorization
        .strip_prefix("Bearer ")
        .is_some_and(is_local_cursor_token)
}

fn is_local_cursor_token(token: &str) -> bool {
    local_token().is_ok_and(|local| local == token)
}

pub(super) fn local_token() -> Result<String> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
        "sub": SUBJECT,
        "email": EMAIL,
        "type": "session",
        "iss": "cursor-client",
        "scope": "openid profile email",
        "exp": 4070908800_u64
    }))?);
    Ok(format!("{header}.{payload}.{SUBJECT}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reinjection_repairs_local_membership_cache() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        inject_if_missing_at(&path).await.unwrap();

        let mut connection = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        sqlx::query("UPDATE ItemTable SET value = 'free' WHERE key = ?")
            .bind("cursorAuth/stripeMembershipType")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("DELETE FROM ItemTable WHERE key = ?")
            .bind("cursorAuth/stripeMembershipAuthId")
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        inject_if_missing_at(&path).await.unwrap();

        let mut connection = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        let membership_type: String =
            sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?")
                .bind("cursorAuth/stripeMembershipType")
                .fetch_one(&mut connection)
                .await
                .unwrap();
        let membership_auth_id: String =
            sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?")
                .bind("cursorAuth/stripeMembershipAuthId")
                .fetch_one(&mut connection)
                .await
                .unwrap();

        assert_eq!(membership_type, MEMBERSHIP_TYPE);
        assert_eq!(membership_auth_id, SUBJECT);
    }

    #[test]
    fn recognizes_only_the_injected_cursor_token() {
        let token = local_token().unwrap();
        assert!(is_local_cursor_token(&token));
        assert!(is_local_cursor_authorization(&format!("Bearer {token}")));
        assert!(!is_local_cursor_authorization(&token));
        assert!(!is_local_cursor_authorization(
            "Bearer official-cursor-token"
        ));
        assert!(!is_local_cursor_token("official-cursor-token"));
        assert!(!is_local_cursor_token(""));
    }

    /// 写进去的每个键都必须出现在清除清单里，否则切到混合/官方模式后会在客户端
    /// 留下半个本地账号。
    #[test]
    fn every_injected_key_is_also_cleared() {
        for (key, _) in injected_values("token") {
            assert!(
                INJECTED_KEYS.contains(&key),
                "{key} is written but never cleared"
            );
        }
    }

    /// 用户自己的登录既不被注入覆盖，也不被清除动到。
    #[tokio::test]
    async fn a_real_account_survives_injection_and_clearing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        write_account(
            &path,
            &[
                ("cursorAuth/accessToken", "real-access-token"),
                ("cursorAuth/cachedEmail", "me@example.com"),
            ],
        )
        .await;

        inject_if_missing_at(&path).await.unwrap();

        assert_eq!(
            read_account(&path, "cursorAuth/accessToken")
                .await
                .as_deref(),
            Some("real-access-token")
        );
        assert_eq!(
            read_account(&path, "cursorAuth/cachedEmail")
                .await
                .as_deref(),
            Some("me@example.com")
        );

        assert!(!clear_injected_account_at(&path).await.unwrap());
        assert_eq!(
            read_account(&path, "cursorAuth/accessToken")
                .await
                .as_deref(),
            Some("real-access-token")
        );
    }

    /// 本地账号连同客户端缓存的展示资料一起被清掉，客户端回到"未登录"。
    #[tokio::test]
    async fn clearing_removes_the_local_account_and_its_derived_profile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        inject_if_missing_at(&path).await.unwrap();
        // 客户端从我们 mock 的账号接口里缓存下来的那一条。
        write_account(
            &path,
            &[(DERIVED_ACCOUNT_KEY, r#"{"authId":"cursor-local-user"}"#)],
        )
        .await;

        assert!(clear_injected_account_at(&path).await.unwrap());

        for key in INJECTED_KEYS.into_iter().chain([DERIVED_ACCOUNT_KEY]) {
            assert!(
                read_account(&path, key).await.is_none(),
                "{key} survived the cleanup"
            );
        }
        // 再清一次没有东西可清。
        assert!(!clear_injected_account_at(&path).await.unwrap());
    }

    #[tokio::test]
    async fn account_state_classifies_the_stored_token() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        assert_eq!(
            read_account_state_at(&path).await.unwrap().state,
            AccountState::Missing
        );

        inject_if_missing_at(&path).await.unwrap();
        let account = read_account_state_at(&path).await.unwrap();
        assert_eq!(account.state, AccountState::Local);
        assert_eq!(account.email.as_deref(), Some(EMAIL));

        write_account(&path, &[("cursorAuth/accessToken", "real-access-token")]).await;
        let account = read_account_state_at(&path).await.unwrap();
        assert_eq!(account.state, AccountState::Official);
    }

    async fn write_account(path: &Path, entries: &[(&str, &str)]) {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
        )
        .execute(&mut connection)
        .await
        .unwrap();
        for (key, value) in entries {
            sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
                .bind(key)
                .bind(value)
                .execute(&mut connection)
                .await
                .unwrap();
        }
    }

    async fn read_account(path: &Path, key: &str) -> Option<String> {
        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(path);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        read_account_value(&mut connection, key).await.unwrap()
    }
}
