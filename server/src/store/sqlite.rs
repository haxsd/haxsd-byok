//! Initializes and configures SQLite storage.
use std::{str::FromStr, time::Duration};

use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    Connection, SqliteConnection, SqlitePool,
};

use crate::Result;

use super::{migrations, writer::WriteCoordinator};

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: SqlitePool,
    /// 清理专用连接池：清理的批次会在 `writes` 前持有一条连接，若和主池共用，
    /// 池被读请求占满时可能与等待连接的写请求互相等待。独立连接池只有一条连接，
    /// 不参与主池配额。
    pub(crate) maintenance_pool: SqlitePool,
    pub(crate) writes: WriteCoordinator,
    /// 后台清理、压缩与页回收的共享进度。
    pub(crate) storage_maintenance: std::sync::Arc<super::storage::StorageMaintenanceState>,
    /// Devin 设置的解析缓存（见 `settings::DevinSettingsCache`）：网关每请求读一次设置，
    /// 不该每次都重新解析 JSON。`Arc` 是必须的——`Store` 会按值克隆，克隆体必须共享同一份
    /// 缓存与同一个版本号。
    pub(crate) devin_settings_cache: std::sync::Arc<super::settings::DevinSettingsCache>,
}

impl Store {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(database_url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(5));
        let database_path = options.get_filename().to_owned();
        let is_new = !database_path.exists();
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .after_release(|connection, _| Box::pin(connection_is_reusable(connection)))
            .connect_with(options.clone())
            .await?;
        // 内存库的第二个连接会是另一个空数据库；内存库直接复用主池。
        let maintenance_pool = if database_path.as_os_str().is_empty()
            || database_path == std::path::Path::new(":memory:")
        {
            pool.clone()
        } else {
            SqlitePoolOptions::new()
                .max_connections(1)
                .after_release(|connection, _| Box::pin(connection_is_reusable(connection)))
                .connect_with(options)
                .await?
        };
        // 新库在第一次建表前启用增量回收；既有库要等用户执行一次整库压缩才真正生效。
        if is_new {
            let mut connection = pool.acquire().await?;
            sqlx::query("PRAGMA auto_vacuum = INCREMENTAL")
                .execute(&mut *connection)
                .await?;
        }
        migrations::run(&pool, &database_path).await?;
        // 旧版的接管布尔在这里换成模式：设置行是用户数据，迁移只做一次，
        // 之后所有代码只认 `cursor_mode`。
        super::settings::migrate_cursor_mode(&pool).await?;
        Ok(Self {
            pool,
            maintenance_pool,
            writes: WriteCoordinator::default(),
            storage_maintenance: std::sync::Arc::new(
                super::storage::StorageMaintenanceState::default(),
            ),
            devin_settings_cache: std::sync::Arc::new(
                super::settings::DevinSettingsCache::default(),
            ),
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// 回池前的裁决：事务没有结束的连接不再交给别人。
///
/// 事务被丢弃时回滚只是排在连接自己的命令队列里，所以先 ping 一次让队列跑完
/// （同一个 worker，先进先出），再判断深度。COMMIT/ROLLBACK 真的失败过（例如磁盘
/// I/O 错误）时深度会留在非零上；这种连接必须关掉，否则之后落在它身上的每个写
/// 入都会以 "attempted to call begin_with at non-zero transaction depth" 失败，
/// 直到进程重启。
async fn connection_is_reusable(connection: &mut SqliteConnection) -> Result<bool, sqlx::Error> {
    if connection.is_in_transaction() {
        let _ = connection.ping().await;
    }
    Ok(!connection.is_in_transaction())
}

#[cfg(test)]
mod tests {
    // `SqliteTransactionManager` 是 sqlx 的隐藏接口：测试需要构造"连接停在事务里"
    // 这个状态，而公开 API（`Transaction`）会把连接借走、结束时回滚，构造不出来。
    use sqlx::{Connection, TransactionManager};

    use super::*;

    /// 一次写失败（例如磁盘 I/O 错误）会把连接留在非零的 transaction depth 上：
    /// 只要这条连接回到池里，之后落在它身上的 `BEGIN IMMEDIATE` 全部报
    /// "attempted to call begin_with at non-zero transaction depth"，直到进程重启。
    /// 连接池必须把这种连接挡在外面，之后的写入照常可用。
    #[tokio::test]
    async fn a_connection_stuck_in_a_transaction_is_never_handed_back() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();

        // 占满池子，把每条连接都推进一个没有结束的事务——失败事务留下的状态。
        let mut stuck = Vec::new();
        for _ in 0..8 {
            let mut connection = store.pool().acquire().await.unwrap();
            sqlx::sqlite::SqliteTransactionManager::begin(&mut connection, None)
                .await
                .unwrap();
            assert!(connection.is_in_transaction());
            stuck.push(connection);
        }
        drop(stuck);

        // 池子交出来的连接必须干净，写入也必须照常成功。
        for _ in 0..8 {
            let connection = store.pool().acquire().await.unwrap();
            assert!(
                !connection.is_in_transaction(),
                "连接池交出了仍然停在事务里的连接"
            );
        }
        let transaction = store.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
        transaction.rollback().await.unwrap();
    }

    /// 维护池是独立的一条连接：同样的「事务未结束」守卫必须挂上。
    ///
    /// 清理/压缩全走维护池，一旦这条连接带着失败事务回池，之后每次
    /// `BEGIN IMMEDIATE` 都会报 non-zero transaction depth，直到进程重启——而池里
    /// 没有第二条连接可换。主池的回归用例覆盖不到这里，所以单独钉一条。
    #[tokio::test]
    async fn the_maintenance_pool_never_hands_back_a_stuck_connection() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();

        {
            let mut connection = store.maintenance_pool.acquire().await.unwrap();
            sqlx::sqlite::SqliteTransactionManager::begin(&mut connection, None)
                .await
                .unwrap();
            assert!(connection.is_in_transaction());
            // 不 commit/rollback，直接丢弃：模拟失败事务留下的连接。
        }

        let connection = store.maintenance_pool.acquire().await.unwrap();
        assert!(
            !connection.is_in_transaction(),
            "维护池交出了仍然停在事务里的连接"
        );
        drop(connection);
        let transaction = store
            .maintenance_pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .unwrap();
        transaction.rollback().await.unwrap();
    }

    /// 正常路径不能被误伤：排队中的回滚在裁决前落地，连接继续复用；
    /// 只有真的停在事务里的连接才被拒。
    #[tokio::test]
    async fn only_connections_with_an_unfinished_transaction_are_rejected() {
        use std::str::FromStr;

        let directory = tempfile::tempdir().unwrap();
        let options = SqliteConnectOptions::from_str(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .unwrap()
        .create_if_missing(true);

        let mut clean = SqliteConnection::connect_with(&options).await.unwrap();
        assert!(connection_is_reusable(&mut clean).await.unwrap());

        // 与驱动的丢弃路径一致：回滚已经排队、还没执行，连接仍然可用。
        let mut queued = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::sqlite::SqliteTransactionManager::begin(&mut queued, None)
            .await
            .unwrap();
        sqlx::sqlite::SqliteTransactionManager::start_rollback(&mut queued);
        assert!(
            connection_is_reusable(&mut queued).await.unwrap(),
            "排队中的回滚应当先落地，连接继续复用"
        );

        // 回滚没有落地（生产里的磁盘 I/O 错误）时，连接必须被拒。
        let mut stuck = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::sqlite::SqliteTransactionManager::begin(&mut stuck, None)
            .await
            .unwrap();
        assert!(
            !connection_is_reusable(&mut stuck).await.unwrap(),
            "仍停在事务里的连接不能再交出去"
        );
    }
}
