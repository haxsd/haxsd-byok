//! Claude Desktop 应用更新的写前日志与中断恢复。
//!
//! 一次 apply 会依次改写 profile、`_meta.json` 和收据。每份文件各自原子替换，但
//! 跨文件的提交不是原子的：进程可能停在任意两次写入之间。日志记录每份文件的前后
//! 内容和待提交收据，下一次状态读取/撤销/重新应用先按日志收敛：
//!
//! - 收据已经提交：只清理日志，绝不回滚；用户之后改过的 profile/meta 保持不变；
//! - 两个文件都已写入新内容但尚未提交：调用方允许时补写收据，否则只收回自己的
//!   半写；
//! - 只写了其中一份：只回滚本应用写下的新内容，保留上一份已提交收据；
//! - 用户在这期间真正改过的文件：原样保留，不覆盖。
//!
//! 日志只服务这一条客户端链路，不是通用事务框架。
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

use super::{atomic_write, read_optional, remove_file_if_exists, same_path};

/// 与收据同目录的写前日志：`<应用数据目录>/claude-takeover/desktop.pending.json`。
pub(crate) const JOURNAL_FILE: &str = "desktop.pending.json";
const JOURNAL_VERSION: u32 = 1;

/// 一次待提交的 Desktop 更新。字段全量记录，进程重启后不需要重新计算。
///
/// 故意不派生 `Debug`：它包含 profile/meta 全文（可能带网关令牌），避免将来被
/// 误格式化进日志。
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PendingUpdate {
    pub version: u32,
    pub meta_path: String,
    pub profile_path: String,
    pub before_profile: Option<String>,
    pub after_profile: String,
    pub before_meta: Option<String>,
    pub after_meta: String,
    /// 已经序列化好的下一份收据；提交时原样落盘。
    pub next_receipt: Value,
}

impl PendingUpdate {
    pub fn new(
        meta_path: String,
        profile_path: String,
        before_profile: Option<String>,
        after_profile: String,
        before_meta: Option<String>,
        after_meta: String,
        next_receipt: Value,
    ) -> Self {
        Self {
            version: JOURNAL_VERSION,
            meta_path,
            profile_path,
            before_profile,
            after_profile,
            before_meta,
            after_meta,
            next_receipt,
        }
    }

    /// 任何客户端文件被改写之前先把日志落盘。
    pub fn begin(&self, state_dir: &Path) -> Result<()> {
        atomic_write(&journal_path(state_dir), &serde_json::to_vec_pretty(self)?)
    }

    pub fn write_profile(&self) -> Result<()> {
        atomic_write(Path::new(&self.profile_path), self.after_profile.as_bytes())
    }

    pub fn write_meta(&self) -> Result<()> {
        atomic_write(Path::new(&self.meta_path), self.after_meta.as_bytes())
    }

    /// 磁盘收据是否已经提交了这份日志对应的更新。这是比“文件仍等于 after_*”
    /// 更强的提交标志：提交后用户改 profile/meta 不会让事务退回未提交。
    pub fn is_committed(&self, receipt: &Path) -> Result<bool> {
        receipt_matches(receipt, &self.next_receipt)
    }

    /// 两个文件都已是新内容后才提交收据，最后移除日志。
    pub fn commit(&self, state_dir: &Path, receipt: &Path) -> Result<()> {
        atomic_write(receipt, &serde_json::to_vec_pretty(&self.next_receipt)?)?;
        remove_journal(state_dir);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// 收据已经提交，只剩日志；只清理日志，绝不回滚。
    Committed,
    /// 两个文件都已写入新内容，收据由恢复补齐。
    Completed,
    /// 只写了一半，已回滚到上一份已提交状态。
    RolledBack,
    /// 用户改过相关文件（或本应用还没来得及写）：保留现状并移除日志；状态检查会
    /// 报告真正的冲突。
    Preserved,
}

/// 读取并校验属于当前安装的日志。别处的日志不碰。
pub(crate) fn pending(
    state_dir: &Path,
    meta: &Path,
    library: &Path,
) -> Result<Option<PendingUpdate>> {
    let Some(text) = read_optional(&journal_path(state_dir))? else {
        return Ok(None);
    };
    let pending: PendingUpdate = serde_json::from_str(&text)?;
    if pending.version != JOURNAL_VERSION {
        return Err(Error::Config(
            "unsupported Claude Desktop pending update".into(),
        ));
    }
    if !same_path(Path::new(&pending.meta_path), meta) {
        return Ok(None);
    }
    let profile = Path::new(&pending.profile_path);
    if !profile
        .parent()
        .is_some_and(|parent| same_path(parent, library))
    {
        return Ok(None);
    }
    Ok(Some(pending))
}

/// 收敛一次停在任意写入间隙的 apply。返回 `None` 表示没有属于当前安装的日志。
///
/// `allow_commit` 控制“未提交但两个文件都已写好”时是否补写收据。apply 可以完成这次
/// 更新；undo 和受管策略下的状态读取只能收回本应用写下的内容，不能替用户完成一次
/// 新的 apply。收据已经提交时不受该参数影响：只清理日志，绝不回滚。
pub(crate) fn recover(
    state_dir: &Path,
    receipt: &Path,
    meta: &Path,
    library: &Path,
    allow_commit: bool,
) -> Result<Option<Recovery>> {
    let Some(pending) = pending(state_dir, meta, library)? else {
        return Ok(None);
    };
    // 磁盘收据与日志里的下一份收据一致，说明三个文件都已提交；之后用户改 profile
    // 或 meta 都是用户后改，不能因为文件不再等于 after 就回滚已提交的事务。
    if pending.is_committed(receipt)? {
        remove_journal(state_dir);
        return Ok(Some(Recovery::Committed));
    }

    let profile = Path::new(&pending.profile_path);
    let profile_now = read_optional(profile)?;
    let meta_now = read_optional(meta)?;
    let profile_after = profile_now.as_deref() == Some(pending.after_profile.as_str());
    let meta_after = meta_now.as_deref() == Some(pending.after_meta.as_str());

    if profile_after && meta_after && allow_commit {
        pending.commit(state_dir, receipt)?;
        return Ok(Some(Recovery::Completed));
    }

    // 未完成：只回滚本应用刚写下的新内容。内容不等于前后任一版本的文件一定是
    // 用户改过的，保持原样。受管策略下也走这里：收回自己的半写，而不是完成新 apply。
    let mut rolled_back = false;
    if profile_after {
        restore(profile, pending.before_profile.as_deref())?;
        rolled_back = true;
    }
    if meta_after {
        restore(meta, pending.before_meta.as_deref())?;
        rolled_back = true;
    }
    remove_journal(state_dir);
    Ok(Some(if rolled_back {
        Recovery::RolledBack
    } else {
        Recovery::Preserved
    }))
}

fn receipt_matches(receipt: &Path, expected: &Value) -> Result<bool> {
    let Some(text) = read_optional(receipt)? else {
        return Ok(false);
    };
    Ok(serde_json::from_str::<Value>(&text).is_ok_and(|current| current == *expected))
}

/// 日志删除是尽力而为：已提交/已回滚的结论不再依赖日志是否立刻消失；删不掉时
/// 下一次状态读取会重试，不能反过来把已提交的配置判成半写。
fn remove_journal(state_dir: &Path) {
    if let Err(error) = remove_file_if_exists(&journal_path(state_dir)) {
        tracing::warn!(%error, "cannot remove the Claude Desktop pending update log");
    }
}

fn journal_path(state_dir: &Path) -> PathBuf {
    state_dir.join(JOURNAL_FILE)
}

fn restore(path: &Path, before: Option<&str>) -> Result<()> {
    match before {
        Some(text) => atomic_write(path, text.as_bytes()),
        None => remove_file_if_exists(path),
    }
}
