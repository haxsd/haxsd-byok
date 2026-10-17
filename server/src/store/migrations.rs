//! Runs embedded SQLite migrations with enough progress data to diagnose startup failures.
use std::{
    borrow::Cow,
    collections::HashSet,
    future::Future,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use sqlx::{
    migrate::{Migration, Migrator},
    Row, SqlitePool,
};

use crate::{Error, Result};

/// 心跳告警间隔：超过这么久还没完成就 warn 一条——**只是告警**，不判失败。
///
/// 慢机器上一条 `CREATE INDEX` 或整表回填可以跑上几分钟，那是正常升级而不是故障；
/// 以前 30 秒硬超时会把这种"慢但正常"的升级判成启动失败。
const MIGRATION_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// 普通迁移的硬超时。
const MIGRATION_STAGE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// 重建类迁移（建替代表 → 拷数据 → 换名）的硬超时：整表复制是最慢的一类，要给足。
const MIGRATION_REBUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

static ALL_MIGRATIONS: Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MigrationLineEndings {
    Lf,
    Crlf,
}

impl MigrationLineEndings {
    fn label(self) -> &'static str {
        match self {
            Self::Lf => "lf",
            Self::Crlf => "crlf",
        }
    }
}

#[derive(Debug)]
struct AppliedMigration {
    version: i64,
    description: String,
    success: bool,
    checksum: Vec<u8>,
    execution_time_ns: i64,
}

fn migrator_with_line_endings(line_endings: MigrationLineEndings) -> Migrator {
    let migrations = ALL_MIGRATIONS
        .iter()
        .map(|migration| {
            let lf_sql = migration.sql.replace("\r\n", "\n");
            let sql = match line_endings {
                MigrationLineEndings::Lf => lf_sql,
                MigrationLineEndings::Crlf => lf_sql.replace('\n', "\r\n"),
            };
            Migration::new(
                migration.version,
                migration.description.clone(),
                migration.migration_type,
                Cow::Owned(sql),
                migration.no_tx,
            )
        })
        .collect::<Vec<_>>();
    Migrator {
        migrations: Cow::Owned(migrations),
        ..Migrator::DEFAULT
    }
}

fn history_matches(migrator: &Migrator, applied: &[AppliedMigration]) -> bool {
    applied.iter().all(|applied| {
        applied.success
            && migrator.iter().any(|known| {
                known.version == applied.version && known.checksum.as_ref() == applied.checksum
            })
    })
}

fn select_migrator(applied: &[AppliedMigration]) -> (Migrator, MigrationLineEndings) {
    let lf = migrator_with_line_endings(MigrationLineEndings::Lf);
    if history_matches(&lf, applied) {
        return (lf, MigrationLineEndings::Lf);
    }

    let crlf = migrator_with_line_endings(MigrationLineEndings::Crlf);
    if history_matches(&crlf, applied) {
        return (crlf, MigrationLineEndings::Crlf);
    }

    // Preserve SQLx's exact validation error for dirty, unknown, or genuinely modified history.
    (lf, MigrationLineEndings::Lf)
}

pub(super) async fn run(pool: &SqlitePool, database_path: &Path) -> Result<()> {
    tracing::info!("running database migrations with diagnostics");
    log_database_files(database_path);
    log_sqlite_state(pool).await?;

    let applied = load_applied_migrations(pool).await?;
    let (migrator, line_endings) = select_migrator(&applied);
    tracing::info!(
        line_endings = line_endings.label(),
        "selected database migration line endings"
    );
    let known_versions = migrator
        .iter()
        .map(|migration| migration.version)
        .collect::<HashSet<_>>();
    let applied_versions = applied
        .iter()
        .map(|migration| migration.version)
        .collect::<HashSet<_>>();
    let known = migrator
        .iter()
        .map(|migration| format!("{:04} {}", migration.version, migration.description))
        .collect::<Vec<_>>();
    let pending = migrator
        .iter()
        .filter(|migration| !applied_versions.contains(&migration.version))
        .map(|migration| format!("{:04} {}", migration.version, migration.description))
        .collect::<Vec<_>>();

    tracing::info!(known = ?known, "embedded database migrations");
    if applied.is_empty() {
        tracing::info!("no applied database migrations were found");
    } else {
        for migration in &applied {
            tracing::info!(
                version = migration.version,
                description = %migration.description,
                success = migration.success,
                execution_time_ns = migration.execution_time_ns,
                checksum_bytes = migration.checksum.len(),
                "applied database migration"
            );
        }
    }
    tracing::info!(pending = ?pending, pending_count = pending.len(), "pending database migrations");

    let invalid_history = applied.iter().any(|applied| {
        let Some(known) = migrator
            .iter()
            .find(|migration| migration.version == applied.version)
        else {
            return true;
        };
        !applied.success || known.checksum.as_ref() != applied.checksum
    }) || applied
        .iter()
        .any(|migration| !known_versions.contains(&migration.version));

    if invalid_history {
        tracing::error!(
            "database migration history is dirty, unknown, or has a checksum mismatch; running SQLx validation for the exact error"
        );
        return run_stage(
            "migration history validation",
            MIGRATION_STAGE_TIMEOUT,
            migrator.run(pool),
        )
        .await;
    }

    for (index, migration) in migrator.iter().enumerate() {
        if applied_versions.contains(&migration.version) {
            continue;
        }

        let stage = format!("{:04} {}", migration.version, migration.description);
        let prefix = Migrator {
            migrations: Cow::Owned(migrator.iter().take(index + 1).cloned().collect()),
            ignore_missing: true,
            ..Migrator::DEFAULT
        };
        run_stage(&stage, stage_timeout(&migration), prefix.run(pool)).await?;
        log_database_files(database_path);
        log_sqlite_state(pool).await?;
    }

    run_stage(
        "final migration history validation",
        MIGRATION_STAGE_TIMEOUT,
        migrator.run(pool),
    )
    .await?;
    tracing::info!("database migrations completed");
    Ok(())
}

/// 这条迁移属于"重建类"吗：`ALTER TABLE … RENAME TO` 是"建替代表 → 拷数据 → 换名"的标志
/// （SQLite 不支持改约束，只能整表重建，所以这一类最慢）。
fn is_rebuild(sql: &str) -> bool {
    sql.to_ascii_uppercase().contains("RENAME TO")
}

fn stage_timeout(migration: &Migration) -> Duration {
    if is_rebuild(&migration.sql) {
        MIGRATION_REBUILD_TIMEOUT
    } else {
        MIGRATION_STAGE_TIMEOUT
    }
}

async fn run_stage<F>(stage: &str, stage_timeout: Duration, future: F) -> Result<()>
where
    F: Future<Output = std::result::Result<(), sqlx::migrate::MigrateError>>,
{
    run_stage_with_limits(stage, MIGRATION_HEARTBEAT_INTERVAL, stage_timeout, future).await
}

async fn run_stage_with_limits<F>(
    stage: &str,
    heartbeat_interval: Duration,
    stage_timeout: Duration,
    future: F,
) -> Result<()>
where
    F: Future<Output = std::result::Result<(), sqlx::migrate::MigrateError>>,
{
    let started = Instant::now();
    let mut heartbeat = tokio::time::interval(heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await;
    let deadline = tokio::time::sleep(stage_timeout);
    tokio::pin!(deadline);
    tokio::pin!(future);

    tracing::info!(
        stage,
        timeout_seconds = stage_timeout.as_secs_f64(),
        "database migration stage started"
    );
    loop {
        tokio::select! {
            result = &mut future => {
                let elapsed_ms = started.elapsed().as_millis();
                return match result {
                    Ok(()) => {
                        tracing::info!(stage, elapsed_ms, "database migration stage completed");
                        Ok(())
                    }
                    Err(error) => {
                        tracing::error!(stage, elapsed_ms, %error, "database migration stage failed");
                        Err(error.into())
                    }
                };
            }
            _ = heartbeat.tick() => {
                tracing::warn!(
                    stage,
                    elapsed_ms = started.elapsed().as_millis(),
                    "database migration stage is still running; the database may be locked by another process"
                );
            }
            _ = &mut deadline => {
                let elapsed_seconds = started.elapsed().as_secs();
                // 文案要说清"这不是数据坏了"：迁移可能仍在库里进行（SQLite 的 DDL 与数据
                // 拷贝都在事务里，进程被杀会回滚），下次启动会重试同一条。
                tracing::error!(
                    stage,
                    elapsed_seconds,
                    timeout_seconds = stage_timeout.as_secs(),
                    "database migration stage timed out; the migration may still be running in the database and the next start will retry it"
                );
                return Err(Error::MigrationTimeout {
                    stage: stage.to_owned(),
                    timeout_seconds: stage_timeout.as_secs(),
                });
            }
        }
    }
}

async fn load_applied_migrations(pool: &SqlitePool) -> Result<Vec<AppliedMigration>> {
    tracing::info!("reading database migration history");
    let table_exists: i64 = sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = '_sqlx_migrations'
         )",
    )
    .fetch_one(pool)
    .await?;
    if table_exists == 0 {
        tracing::info!("database migration history table does not exist");
        return Ok(Vec::new());
    }

    let rows = sqlx::query(
        "SELECT version, description, success, checksum, execution_time
         FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await?;
    tracing::info!(row_count = rows.len(), "database migration history loaded");
    rows.into_iter()
        .map(|row| {
            Ok(AppliedMigration {
                version: row.try_get("version")?,
                description: row.try_get("description")?,
                success: row.try_get("success")?,
                checksum: row.try_get("checksum")?,
                execution_time_ns: row.try_get("execution_time")?,
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
        .map_err(Into::into)
}

async fn log_sqlite_state(pool: &SqlitePool) -> Result<()> {
    tracing::info!(
        pool_size = pool.size(),
        pool_idle = pool.num_idle(),
        "reading SQLite runtime state"
    );
    let sqlite_version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(pool)
        .await?;
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(pool)
        .await?;
    let locking_mode: String = sqlx::query_scalar("PRAGMA locking_mode")
        .fetch_one(pool)
        .await?;
    let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(pool)
        .await?;
    let busy_timeout_ms: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(pool)
        .await?;
    let schema_version: i64 = sqlx::query_scalar("PRAGMA schema_version")
        .fetch_one(pool)
        .await?;
    let user_version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(pool)
        .await?;
    let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(pool)
        .await?;
    let freelist_count: i64 = sqlx::query_scalar("PRAGMA freelist_count")
        .fetch_one(pool)
        .await?;

    tracing::info!(
        %sqlite_version,
        %journal_mode,
        %locking_mode,
        foreign_keys,
        busy_timeout_ms,
        schema_version,
        user_version,
        page_size,
        page_count,
        freelist_count,
        "SQLite runtime state"
    );
    Ok(())
}

fn log_database_files(database_path: &Path) {
    tracing::info!(
        database_path = %database_path.display(),
        process_id = std::process::id(),
        "SQLite database files"
    );
    log_file("database", database_path);
    log_file("wal", &sidecar_path(database_path, "-wal"));
    log_file("shm", &sidecar_path(database_path, "-shm"));
    log_file("journal", &sidecar_path(database_path, "-journal"));
}

fn log_file(kind: &str, path: &Path) {
    match std::fs::metadata(path) {
        Ok(metadata) => tracing::info!(
            kind,
            path = %path.display(),
            exists = true,
            size_bytes = metadata.len(),
            readonly = metadata.permissions().readonly(),
            "SQLite file state"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => tracing::info!(
            kind,
            path = %path.display(),
            exists = false,
            "SQLite file state"
        ),
        Err(error) => {
            tracing::warn!(kind, path = %path.display(), %error, "failed to read SQLite file state")
        }
    }
}

fn sidecar_path(database_path: &Path, suffix: &str) -> PathBuf {
    let mut path = database_path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    #[test]
    fn normalized_checksums_match_the_published_lf_and_windows_crlf_migrations() {
        let lf = migrator_with_line_endings(MigrationLineEndings::Lf);
        let crlf = migrator_with_line_endings(MigrationLineEndings::Crlf);
        let lf_initial = lf.iter().find(|migration| migration.version == 1).unwrap();
        let crlf_initial = crlf
            .iter()
            .find(|migration| migration.version == 1)
            .unwrap();

        assert_eq!(
            hex::encode(lf_initial.checksum.as_ref()),
            "ddf1bc573e460bfd93ea50b60d003e8fc6bb9a1b32de71139cb0fc0d898e88c401c08c8a8b57abbe68f55927e7f004d9"
        );
        assert_eq!(
            hex::encode(crlf_initial.checksum.as_ref()),
            "7c5995693dbd5f9d50880fc874784cb67c499762abcee4a562b54c9afd8239bae125074696b78f90aca0fc136b802a2b"
        );
    }

    #[tokio::test]
    async fn lf_and_crlf_migration_histories_upgrade_without_rewriting_checksums() {
        for line_endings in [MigrationLineEndings::Lf, MigrationLineEndings::Crlf] {
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .unwrap();
            let historical = migrator_with_line_endings(line_endings);
            let first_four = Migrator {
                migrations: Cow::Owned(historical.iter().take(4).cloned().collect()),
                ..Migrator::DEFAULT
            };
            first_four.run(&pool).await.unwrap();
            let checksum_before: Vec<u8> =
                sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 1")
                    .fetch_one(&pool)
                    .await
                    .unwrap();

            run(&pool, Path::new("line-ending-compatibility.db"))
                .await
                .unwrap();

            let checksum_after: Vec<u8> =
                sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 1")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            let versions: Vec<i64> =
                sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            let checkpoint_table_exists: i64 = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master
                    WHERE type = 'table' AND name = 'conversation_checkpoints'
                 )",
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            let argument_error_column_exists: i64 = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info('tool_round_calls')
                    WHERE name = 'argument_error'
                 )",
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            let client_source_column_exists: i64 = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info('llm_calls')
                    WHERE name = 'client_source'
                 )",
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            let client_source_index_exists: i64 = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master
                    WHERE type = 'index' AND name = 'llm_calls_client_source'
                 )",
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            let message_image_table_exists: i64 = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master
                    WHERE type = 'table' AND name = 'message_image_blobs'
                 )",
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            let artifact_byte_count_exists: i64 = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info('cursor_run_trace_artifacts')
                    WHERE name = 'byte_count'
                 )",
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            let runs_status_index_exists: i64 = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master
                    WHERE type = 'index' AND name = 'runs_status_updated'
                 )",
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            let llm_calls_conversation_index_exists: i64 = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master
                    WHERE type = 'index' AND name = 'llm_calls_conversation_created'
                 )",
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            let runs_parent_indexes_exist: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name IN ('runs_parent_run', 'runs_parent_tool_call')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            let checkpoint_messages_message_index_exists: i64 = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master
                    WHERE type = 'index' AND name = 'checkpoint_messages_message'
                 )",
            )
            .fetch_one(&pool)
            .await
            .unwrap();

            assert_eq!(checksum_after, checksum_before);
            assert_eq!(
                versions,
                vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22]
            );
            assert_eq!(checkpoint_table_exists, 1);
            assert_eq!(argument_error_column_exists, 1);
            assert_eq!(client_source_column_exists, 1);
            assert_eq!(client_source_index_exists, 1);
            assert_eq!(message_image_table_exists, 1);
            assert_eq!(artifact_byte_count_exists, 1);
            assert_eq!(runs_status_index_exists, 1);
            assert_eq!(llm_calls_conversation_index_exists, 1);
            assert_eq!(runs_parent_indexes_exist, 2);
            assert_eq!(checkpoint_messages_message_index_exists, 1);
        }
    }

    /// 删除父行时，SQLite 必须到子表确认"没有引用它"。子表的外键列没有索引时，这个检查
    /// 退化成一次全表扫描，代价随"删掉的父行数 × 子表行数"增长：会话回收按会话批量删
    /// checkpoint 与 message，单条 DELETE 因此从毫秒级涨到分钟级，60 秒一拍的维护事务
    /// 一直占着写锁，桌面端的保存与调用落库全部排队超时。迁移 0015 补齐的就是这些索引；
    /// 这条测试保证之后新增的外键同样带着子表侧索引。
    #[tokio::test]
    async fn every_foreign_key_child_column_has_a_usable_index() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        run(&pool, Path::new("foreign-key-child-indexes.db"))
            .await
            .unwrap();

        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let mut missing = Vec::new();
        for table in tables {
            // id 相同的行属于同一个（可能是复合的）外键，按 seq 还原列顺序。
            let columns: Vec<(i64, String)> = sqlx::query_as(&format!(
                "SELECT id, \"from\" FROM pragma_foreign_key_list('{table}') ORDER BY id, seq"
            ))
            .fetch_all(&pool)
            .await
            .unwrap();
            // 部分索引不覆盖所有行，外键检查用不上，所以只看完整索引。
            let index_names: Vec<String> = sqlx::query_scalar(&format!(
                "SELECT name FROM pragma_index_list('{table}') WHERE partial = 0"
            ))
            .fetch_all(&pool)
            .await
            .unwrap();
            let mut available: Vec<Vec<String>> = Vec::new();
            for name in index_names {
                available.push(
                    sqlx::query_scalar(&format!(
                        "SELECT name FROM pragma_index_info('{name}')
                         WHERE name IS NOT NULL ORDER BY seqno"
                    ))
                    .fetch_all(&pool)
                    .await
                    .unwrap(),
                );
            }

            let mut start = 0;
            while start < columns.len() {
                let id = columns[start].0;
                let mut end = start;
                while end < columns.len() && columns[end].0 == id {
                    end += 1;
                }
                let child: Vec<String> = columns[start..end]
                    .iter()
                    .map(|(_, column)| column.clone())
                    .collect();
                // 索引的前几列必须正好是这个外键的子列，顺序也要一致。
                if !available
                    .iter()
                    .any(|index| index.starts_with(child.as_slice()))
                {
                    missing.push(format!("{table}({})", child.join(", ")));
                }
                start = end;
            }
        }

        assert!(
            missing.is_empty(),
            "这些外键的子表侧没有可用索引，删除父行会逐行全表扫描：{missing:?}"
        );
    }

    /// 迁移 0013 会给已有记录补来源：前缀可确认的按前缀补，能对上 runs 的算 Cursor，
    /// 其余保持 internal——不能把无法确认的历史算成某个客户端。
    #[tokio::test]
    async fn client_source_backfill_attributes_only_what_history_can_prove() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let historical = migrator_with_line_endings(MigrationLineEndings::Lf);
        let before_client_source = Migrator {
            migrations: Cow::Owned(historical.iter().take(12).cloned().collect()),
            ..Migrator::DEFAULT
        };
        before_client_source.run(&pool).await.unwrap();

        sqlx::query("INSERT INTO conversations(conversation_id, updated_at_ms) VALUES ('c1', 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO conversation_checkpoints(conversation_id, state_digest, created_at_ms) VALUES ('c1', zeroblob(32), 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO runs(run_id, conversation_id, base_checkpoint_id, head_checkpoint_id, run_kind, status, created_at_ms, updated_at_ms) VALUES ('run-1', 'c1', 1, 1, 'agent', 'completed', 1, 1)")
            .execute(&pool)
            .await
            .unwrap();
        for (call_id, run_id) in [
            ("devin:exec-1", "unknown"),
            ("devin-call:2", "unknown"),
            ("cursor-run:3", "run-1"),
            ("commit-message-4", "unknown"),
            ("mystery-call", "unknown"),
        ] {
            sqlx::query(
                "INSERT INTO llm_calls(call_id, run_id, conversation_id, provider_call_index, provider_type, provider_url,
                    request_type, request_url, model_id, display_name, status, created_at_ms, message_count, tool_count, detailed)
                 VALUES (?, ?, 'c1', 0, 'openai', 'https://example.com', 'openai', 'https://example.com/v1', 'm', 'M', 'completed', 1, 1, 0, 0)",
            )
            .bind(call_id)
            .bind(run_id)
            .execute(&pool)
            .await
            .unwrap();
        }

        run(&pool, Path::new("client-source-backfill.db"))
            .await
            .unwrap();

        let sources: Vec<(String, String)> =
            sqlx::query_as("SELECT call_id, client_source FROM llm_calls ORDER BY call_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            sources,
            vec![
                ("commit-message-4".to_string(), "cursor".to_string()),
                ("cursor-run:3".to_string(), "cursor".to_string()),
                ("devin-call:2".to_string(), "devin".to_string()),
                ("devin:exec-1".to_string(), "devin".to_string()),
                ("mystery-call".to_string(), "internal".to_string()),
            ]
        );
    }

    /// 迁移 0017 会给已有消息里的工具图片补一条引用登记。不补的话，这些图片在无根
    /// 窗口之后会被回收趟当成垃圾删掉，而消息还指着它们。
    #[tokio::test]
    async fn existing_message_images_are_registered_by_the_migration() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let historical = migrator_with_line_endings(MigrationLineEndings::Lf);
        let before_message_images = Migrator {
            migrations: Cow::Owned(historical.iter().take(16).cloned().collect()),
            ..Migrator::DEFAULT
        };
        before_message_images.run(&pool).await.unwrap();

        // 32 字节全零的 BlobID 的 base64：迁移只搬 payload 里的文本，不校验它。
        let blob_id = format!("{}=", "A".repeat(43));
        sqlx::query("INSERT INTO conversations(conversation_id, updated_at_ms) VALUES ('c1', 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO messages(conversation_id, message_id, role, origin, payload_json, created_at_ms)
             VALUES ('c1', 'image-message', 'tool', 'tool', ?, 1)",
        )
        .bind(format!(
            "{{\"content\":{{\"kind\":\"tool_result\",\"image\":{{\"blob_id\":\"{blob_id}\"}}}}}}"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages(conversation_id, message_id, role, origin, payload_json, created_at_ms)
             VALUES ('c1', 'text-message', 'user', 'user', '{\"content\":{\"kind\":\"parts\"}}', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        run(&pool, Path::new("message-image-backfill.db"))
            .await
            .unwrap();

        let registrations: Vec<(String, String)> =
            sqlx::query_as("SELECT conversation_id, blob_id FROM message_image_blobs")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(registrations, vec![("c1".to_string(), blob_id)]);
    }

    /// 迁移 0018 会用内容块的实际正文长度回填已有工件的 `byte_count`：NULL / 0 会被
    /// 当成"没有正文"，让存储卡片上的估算凭空变小。
    #[tokio::test]
    async fn existing_trace_artifacts_get_their_body_size_backfilled() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let historical = migrator_with_line_endings(MigrationLineEndings::Lf);
        let before_byte_count = Migrator {
            migrations: Cow::Owned(historical.iter().take(17).cloned().collect()),
            ..Migrator::DEFAULT
        };
        before_byte_count.run(&pool).await.unwrap();

        sqlx::query("INSERT INTO blobs(blob_id, data, created_at_ms, last_used_at_ms) VALUES (zeroblob(32), X'0102030405', 1, 1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO cursor_run_traces(request_id, route, status, received_at_ms)
             VALUES ('r1', 'local_byok', 'completed', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO cursor_run_trace_artifacts(
                request_id, seq, artifact_type, source, blob_id, metadata_json, created_at_ms
             ) VALUES ('r1', 0, 'run_sse_chunk', 'cursor_client', zeroblob(32), '{}', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        run(&pool, Path::new("artifact-byte-count-backfill.db"))
            .await
            .unwrap();

        let byte_count: i64 = sqlx::query_scalar(
            "SELECT byte_count FROM cursor_run_trace_artifacts WHERE request_id = 'r1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(byte_count, 5);
    }

    #[test]
    fn sqlite_sidecar_paths_preserve_the_database_path() {
        let database = Path::new(r"C:\Users\Test User\cursor-byok.db");
        assert_eq!(
            sidecar_path(database, "-wal"),
            PathBuf::from(r"C:\Users\Test User\cursor-byok.db-wal")
        );
    }

    /// 三态之一：迁移正常完成时，心跳只是背景噪声，绝不影响结果。
    ///
    /// 心跳间隔被压到 1ms、完成时间远大于它，所以这条用例真的会跳很多次心跳。
    #[tokio::test]
    async fn heartbeats_do_not_fail_a_migration_stage_that_completes() {
        let result = run_stage_with_limits(
            "0001 initial",
            Duration::from_millis(1),
            Duration::from_secs(30),
            async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(())
            },
        )
        .await;

        assert!(result.is_ok(), "心跳告警不是失败: {result:?}");
    }

    /// 三态之三：硬超时到了才失败，而且错误里带着阶段名与预算秒数（日志另有"仍在进行、
    /// 下次启动重试"的说明）。
    #[tokio::test]
    async fn a_stage_that_exceeds_its_budget_fails_with_the_stage_and_budget() {
        let result = run_stage_with_limits(
            "0004 flatten model configuration",
            Duration::from_millis(1),
            Duration::from_millis(15),
            std::future::pending(),
        )
        .await;

        assert!(
            matches!(
                result,
                Err(Error::MigrationTimeout { ref stage, timeout_seconds })
                    if stage == "0004 flatten model configuration" && timeout_seconds == 0
            ),
            "{result:?}"
        );
    }

    /// 分级预算：重建类迁移（`ALTER TABLE … RENAME TO`）给半小时，普通迁移五分钟。
    ///
    /// 这一条同时钉住仓库里的真实分类：`0004` / `0006` / `0007` 是重建类，其余不是——
    /// 以后新增重建迁移忘了分类，这里会红。
    #[test]
    fn rebuild_migrations_get_the_long_budget() {
        assert_eq!(MIGRATION_HEARTBEAT_INTERVAL, Duration::from_secs(30));
        assert_eq!(MIGRATION_STAGE_TIMEOUT, Duration::from_secs(5 * 60));
        assert_eq!(MIGRATION_REBUILD_TIMEOUT, Duration::from_secs(30 * 60));

        let budget = |version: i64| {
            let migration = ALL_MIGRATIONS
                .iter()
                .find(|migration| migration.version == version)
                .unwrap_or_else(|| panic!("migration {version} must exist"));
            stage_timeout(migration)
        };
        for version in [4, 6, 7] {
            assert_eq!(
                budget(version),
                MIGRATION_REBUILD_TIMEOUT,
                "迁移 {version} 是重建类"
            );
        }
        for version in [1, 2, 3, 5, 8, 20, 21] {
            assert_eq!(budget(version), MIGRATION_STAGE_TIMEOUT, "迁移 {version}");
        }
        assert!(!is_rebuild("CREATE INDEX IF NOT EXISTS i ON t(c)"));
        assert!(is_rebuild("ALTER TABLE t RENAME TO t_old"));
    }

    /// 按 `message_id` 查身份必须走 `checkpoint_messages_message`；不按它查时不该用这条索引。
    ///
    /// `chain_message` 的递归 CTE 每一跳都按 `message_id` 定位身份列（05/P2-4 之后不再解码
    /// payload），而这张表既有的三个索引没有一个以 `message_id` 打头（PK 是
    /// `(checkpoint_id, ordinal)`、UNIQUE 是 `(checkpoint_id, message_id)`、0015 的外键子索引是
    /// `(conversation_id, message_id)`），所以以前只能全表扫——而表只增不减。
    ///
    /// 顺带量一次写入成本：这条索引让每次 INSERT 多维护一个 B-tree，用"带索引插入 N 行 vs
    /// 删掉索引再插 N 行"对照（上下界都给足余量，避免机器负载导致的假红），实测数字打印在
    /// 断言消息里，随 `--nocapture` 可见。
    #[tokio::test]
    async fn message_identity_lookups_search_their_index_and_writes_stay_cheap() {
        let directory = tempfile::tempdir().unwrap();
        // 走生产路径建库：`Store::connect` 跑完全部迁移（这条索引就是迁移 0022 建的）并打开
        // 外键。所以身份数据的父行也要造齐——`checkpoint_messages` 有两条外键：
        // `checkpoint_id` → `conversation_checkpoints`、`(conversation_id, message_id)` → `messages`。
        let store = crate::store::Store::connect(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let pool = store.pool();
        sqlx::query("INSERT INTO conversations(conversation_id, updated_at_ms) VALUES ('conv', 1)")
            .execute(pool)
            .await
            .unwrap();
        let checkpoint: i64 = sqlx::query_scalar(
            "INSERT INTO conversation_checkpoints(conversation_id, state_digest, created_at_ms)
             VALUES ('conv', zeroblob(32), 1) RETURNING checkpoint_id",
        )
        .fetch_one(pool)
        .await
        .unwrap();

        // 造点数据再 ANALYZE：空表上规划器没有统计，计划会随启发式摇摆。
        for ordinal in 0..200_i64 {
            insert_checkpoint_message(pool, checkpoint, ordinal, &format!("seed-{ordinal}")).await;
        }
        sqlx::query("ANALYZE").execute(pool).await.unwrap();

        let hit = plan_of(
            pool,
            "SELECT conversation_id, message_id FROM checkpoint_messages WHERE message_id = ?",
            &["message-1"],
        )
        .await;
        assert!(hit.contains("checkpoint_messages_message"), "{hit}");
        assert!(!hit.contains("SCAN checkpoint_messages"), "{hit}");

        let miss = plan_of(
            pool,
            "SELECT message_id FROM checkpoint_messages WHERE checkpoint_id = ?",
            &[],
        )
        .await;
        assert!(
            !miss.contains("checkpoint_messages_message"),
            "只按 checkpoint_id 查不该用 message_id 索引：{miss}"
        );

        // 两轮各插 200 行，列宽与行数一致，只差"有没有这条索引"；断言余量给足（真正的"明显
        // 变化"至少一个数量级），避免机器负载导致假红。
        let with_index = insert_round(pool, checkpoint, 200, 200).await;
        sqlx::query("DROP INDEX checkpoint_messages_message")
            .execute(pool)
            .await
            .unwrap();
        let without_index = insert_round(pool, checkpoint, 400, 200).await;
        sqlx::query("CREATE INDEX checkpoint_messages_message ON checkpoint_messages(message_id)")
            .execute(pool)
            .await
            .unwrap();
        println!("write cost: 200 rows with index {with_index:?}, without index {without_index:?}");

        assert!(
            with_index.as_secs_f64() <= without_index.as_secs_f64() * 8.0 + 0.05,
            "带索引插入 200 行 {with_index:?} vs 不带索引 {without_index:?}：写入成本不该有明显变化"
        );
    }

    async fn insert_checkpoint_message(
        pool: &SqlitePool,
        checkpoint_id: i64,
        ordinal: i64,
        message_id: &str,
    ) {
        // 外键是打开的：先插父行 `messages`，再插身份行 `checkpoint_messages`。
        sqlx::query(
            "INSERT INTO messages(conversation_id, message_id, role, origin, payload_json, created_at_ms)
             VALUES ('conv', ?, 'user', 'client', '{}', 1)
             ON CONFLICT DO NOTHING",
        )
        .bind(message_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO checkpoint_messages(checkpoint_id, ordinal, conversation_id, message_id)
             VALUES (?, ?, 'conv', ?)",
        )
        .bind(checkpoint_id)
        .bind(ordinal)
        .bind(message_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_round(
        pool: &SqlitePool,
        checkpoint_id: i64,
        first_ordinal: i64,
        rows: i64,
    ) -> Duration {
        let started = Instant::now();
        for ordinal in first_ordinal..first_ordinal + rows {
            insert_checkpoint_message(pool, checkpoint_id, ordinal, &format!("bulk-{ordinal}"))
                .await;
        }
        started.elapsed()
    }

    async fn plan_of(pool: &SqlitePool, sql: &str, binds: &[&str]) -> String {
        let explain = format!("EXPLAIN QUERY PLAN {sql}");
        let mut query = sqlx::query(&explain);
        for value in binds {
            query = query.bind(*value);
        }
        query
            .fetch_all(pool)
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<String, _>("detail").unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn stalled_migration_stage_returns_a_timeout_error() {
        let result = run_stage_with_limits(
            "0006 rename revisions to checkpoints",
            Duration::from_millis(2),
            Duration::from_millis(10),
            std::future::pending(),
        )
        .await;

        assert!(matches!(
            result,
            Err(Error::MigrationTimeout { stage, .. })
                if stage == "0006 rename revisions to checkpoints"
        ));
    }
}
