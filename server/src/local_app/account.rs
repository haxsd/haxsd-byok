//! Integrates local Cursor account state.
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::json;
use sqlx::{Connection, Row, SqliteConnection};

use crate::{Error, Result};

/// 清账号时等 Cursor 的写锁的上界。
///
/// 这条路径在退出与被强杀的收尾里跑，而 Cursor 自己也在写同一个 `state.vscdb`。
/// sqlx 默认等 5 秒：那正好把"退出后马上重开"压在实例锁重试窗口的边界上（用户看到的是
/// 一次静默的启动失败）。清一个账号不值得等这么久——等不到就下一轮再清，状态刷新与
/// 下一次退出都会重试。
const CLEAR_BUSY_TIMEOUT: Duration = Duration::from_secs(1);

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

/// 客户端里没有账号时补一个本地账号，让它可以带着这个账号使用本地模型。
///
/// 返回是否真的写了东西。读状态每几秒走一次这条路（见 `CursorHarness::status`），
/// 所以"已经是我们要写的那一份"必须是彻底的只读判断：既不重写客户端的
/// `state.vscdb`（那个库客户端自己也在写），也不让"注入本地账号"在日志里刷屏。
pub async fn inject_if_missing() -> Result<bool> {
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

/// 清账号用的连接选项：写锁最多等 `CLEAR_BUSY_TIMEOUT`，而不是 sqlx 默认的 5 秒。
fn clear_connection_options(path: &Path) -> sqlx::sqlite::SqliteConnectOptions {
    sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .busy_timeout(CLEAR_BUSY_TIMEOUT)
}

async fn clear_injected_account_at(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut connection = SqliteConnection::connect_with(&clear_connection_options(path)).await?;
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

async fn inject_if_missing_at(path: &Path) -> Result<bool> {
    // 先只读判断：这条路径每几秒被状态刷新走一次（`CursorHarness::status` 的补齐路径），
    // 而客户端自己也在写这个库。注释承诺的"彻底的只读判断"必须真的只读——以前这里一上来
    // 就 `create_dir_all` + 可写打开（`create_if_missing`）+ 无条件 `CREATE TABLE IF NOT
    // EXISTS`，稳态下也要走一遍可写路径：只读 ACL / 只读属性的 `state.vscdb` 连接直接失败，
    // 失败还会顺着 F2 那条链把整个状态接口带下去。
    if let Some(false) = already_injected_at(path).await? {
        return Ok(false);
    }
    // 确实要写（库/表还不存在，或有键缺失、被改成别的值）才动可写连接与 DDL。
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
        return Ok(false);
    }
    let values = injected_values(&token);
    if holds_our_values(&mut connection, &values).await? {
        return Ok(false);
    }

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
    Ok(true)
}

/// 只读探测：库能打开、表在、而且已经是我们会写的那一份（或里面是别人的账号）时回答
/// `Some(false)`（什么都不用做）。
///
/// `None` 表示"读不出来，交给写路径"：文件还不存在（用户从没运行过 Cursor）、表还没建、
/// 库损坏或只读 ACL 导致连接失败——这些都要由写路径去建目录/建库/建表，并给出真正的错误。
/// 刻意不在这里报错：探测失败只有"要不要写"这一个含义，报错点应该在真正动手的地方。
async fn already_injected_at(path: &Path) -> Result<Option<bool>> {
    if !path.exists() {
        return Ok(None);
    }
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .read_only(true);
    let mut connection = match SqliteConnection::connect_with(&options).await {
        Ok(connection) => connection,
        Err(error) => {
            tracing::debug!(
                %error,
                "cursor state database is not readable; injection will open it read-write"
            );
            return Ok(None);
        }
    };
    let account = sqlx::query("SELECT CAST(value AS TEXT) AS value FROM ItemTable WHERE key = ?")
        .bind("cursorAuth/accessToken")
        .fetch_optional(&mut connection)
        .await;
    let account = match account {
        Ok(account) => account,
        // 表还不存在（客户端没跑过）或库读不了。
        Err(_) => return Ok(None),
    };
    let token = local_token()?;
    if account.is_some_and(|row| {
        row.try_get::<String, _>("value")
            .is_ok_and(|value| !value.trim().is_empty() && value != token)
    }) {
        return Ok(Some(false));
    }
    let values = injected_values(&token);
    match holds_our_values(&mut connection, &values).await {
        Ok(true) => Ok(Some(false)),
        Ok(false) | Err(_) => Ok(None),
    }
}

/// 客户端里这七个键是不是已经正好是我们会写的那一份。
///
/// 是的话什么都不用做：不重写库、不记日志。半个本地账号（键缺失或被改成别的值）
/// 仍然按原来的方式补齐——上次写入中途失败、或客户端自己改了会员缓存，都要修回来。
async fn holds_our_values(
    connection: &mut SqliteConnection,
    values: &[(&'static str, String)],
) -> Result<bool> {
    for (key, expected) in values {
        if read_account_value(connection, key).await?.as_deref() != Some(expected.as_str()) {
            return Ok(false);
        }
    }
    Ok(true)
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

    /// 读状态每几秒调一次注入：本地账号已经是我们写的那一份时，它必须什么都不做。
    ///
    /// 这里用返回的"是否写过"判断：写一次要开客户端的 `state.vscdb` 并提交一个写
    /// 事务，而客户端自己也在写那个库；重写还会让日志每次刷新都多一行。
    #[tokio::test]
    async fn a_second_injection_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");

        assert!(inject_if_missing_at(&path).await.unwrap(), "第一次要写");
        assert!(
            !inject_if_missing_at(&path).await.unwrap(),
            "已经是我们的账号，第二次不能再写"
        );

        // 客户端自己改了会员缓存：这时候必须重新补齐，不能因为"账号还在"就跳过。
        let mut connection = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        sqlx::query("UPDATE ItemTable SET value = 'free' WHERE key = ?")
            .bind("cursorAuth/stripeMembershipType")
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        assert!(inject_if_missing_at(&path).await.unwrap(), "被改坏要补回来");
    }

    /// 稳态判定必须来自只读探测：库里已经是我们那份（或有别人的账号）时探测就回答"什么都不用
    /// 做"，只有"读不出来 / 还不存在"才交给可写路径。
    ///
    /// 旧实现没有这一步：稳态下也会 `create_dir_all` + 可写打开（`create_if_missing`）+ 无条件
    /// `CREATE TABLE IF NOT EXISTS`。实测（Python sqlite3 与 sqlx 同一份 C 库）SQLite 对带
    /// `FILE_ATTRIBUTE_READONLY` 的文件会**静默降级**成只读打开，所以"结果是 Ok(false)"这一点
    /// 两边一样——差别在于稳态要不要写权限、要不要 DDL、会不会跟客户端抢锁。因此这条用例钉的是
    /// 判据本身：探测的返回值必须能区分"可以跳过"与"必须走写路径"。
    #[tokio::test]
    async fn the_probe_answers_whether_injection_can_be_skipped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");

        // 文件还不存在（用户没运行过 Cursor）：没有可跳过的余地。
        assert!(matches!(already_injected_at(&path).await, Ok(None)));

        inject_if_missing_at(&path).await.unwrap();
        assert!(
            matches!(already_injected_at(&path).await, Ok(Some(false))),
            "已经是我们写的那一份：只读探测就能下结论"
        );

        let mut connection = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        // 用户自己的账号：同样什么都不用做。
        sqlx::query("UPDATE ItemTable SET value = 'a-real-cursor-token' WHERE key = ?")
            .bind("cursorAuth/accessToken")
            .execute(&mut connection)
            .await
            .unwrap();
        assert!(matches!(already_injected_at(&path).await, Ok(Some(false))));

        // 半个本地账号（键被删掉）：必须回到写路径去补。
        sqlx::query("UPDATE ItemTable SET value = ? WHERE key = ?")
            .bind(local_token().unwrap())
            .bind("cursorAuth/accessToken")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("DELETE FROM ItemTable WHERE key = ?")
            .bind("cursorAuth/stripeMembershipType")
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);
        assert!(
            matches!(already_injected_at(&path).await, Ok(None)),
            "缺键要重新补齐，不能在只读判断里被跳过"
        );
        assert!(inject_if_missing_at(&path).await.unwrap(), "补回来");
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

    /// Cursor 正握着 `state.vscdb` 的写锁时，清账号必须很快放弃，而不是等满 sqlx 默认的
    /// 5 秒：这条路径在退出与被强杀的收尾里跑，用户在"退出后马上重开"的窗口里等不起。
    #[tokio::test]
    async fn clearing_gives_up_quickly_when_cursor_holds_the_write_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");
        inject_if_missing_at(&path).await.unwrap();

        // 配置本身：清除用的连接必须只等 1 秒（sqlx 的默认值是 5 秒）。这一条与机器负载
        // 无关，是这条用例真正要钉住的东西。
        let mut probe = SqliteConnection::connect_with(&clear_connection_options(&path))
            .await
            .unwrap();
        let configured: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(&mut probe)
            .await
            .unwrap();
        assert_eq!(configured, CLEAR_BUSY_TIMEOUT.as_millis() as i64);
        drop(probe);

        // 行为：另一个连接占着写锁（等价于 Cursor 自己正在写那个库）时，清除要报错，
        // 而且不能等到 sqlx 默认的 5 秒。
        let mut holder = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut holder)
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let result = clear_injected_account_at(&path).await;
        let elapsed = started.elapsed();

        sqlx::query("ROLLBACK").execute(&mut holder).await.unwrap();
        assert!(
            result.is_err(),
            "写锁被占时应当报错，而不是悄悄跳过: {result:?}"
        );
        // 4 秒：留出被调度/杀软拖慢的余量，同时把"等满 sqlx 默认的 5 秒"挡在外面。
        assert!(
            elapsed < Duration::from_secs(4),
            "等满了 sqlx 默认的 5 秒上界: {elapsed:?}"
        );
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
