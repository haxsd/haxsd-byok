//! Spills full tool results that the model-visible size gate truncated, and
//! archives the conversation a compaction shadowed.
//!
//! Both artifacts are best effort: a failed write or a size-cap decision must
//! never fail a tool result or a run.
//!
//! **Lifetime follows the reference, not the wall clock.** A spill file is
//! named after its `call_id` and an archive after its `conversation_id`; the
//! maintenance loop deletes a file once that row is gone (conversations recycle
//! after a day of inactivity). Before this, files were pruned on write after 7
//! days or a file-count ceiling, while the truncation notice and the summary
//! slot pointing at them stayed in the persisted history forever — a model that
//! followed the hint a week later got "file not found". Names written by older
//! versions cannot be resolved; those fall back to the 7-day window.
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::config;

use super::CanonicalMessage;

pub(crate) const SPILL_DIR: &str = "tool-results";
pub(crate) const COMPACTION_ARCHIVE_DIR: &str = "compaction-history";
/// 无法解析出引用的文件（旧命名、跨版本遗留）最多保留多少天。
///
/// 能解析出引用的文件按"引用还在不在"决定生死，与天数无关；这个窗口只兜底
/// 那些解析不出来的文件，以及引用表读不到时的安全网。
pub(crate) const RETENTION_DAYS: i64 = 7;
const RETENTION: Duration = Duration::from_secs(RETENTION_DAYS as u64 * 24 * 60 * 60);
/// 新写的文件先不判死：工具结果落盘发生在调用行提交之前，维护循环恰好在中间跑
/// 一趟时不能把还活着的结果删掉。
const GRACE: Duration = Duration::from_secs(60 * 60);
/// 数量上限：重度使用一周也不该无限增长。超出后从最老的开始删——即使它还有引用，
/// 磁盘有界优先。
const SPILL_MAX_FILES: usize = 2_048;
const ARCHIVE_MAX_FILES: usize = 256;
/// 目录字节上限：数量上限之外的第二道闸（单文件最多 8 MB）。
const MAX_DIRECTORY_BYTES: u64 = 512 * 1024 * 1024;
/// Largest single tool result worth spilling. Bigger payloads are usually
/// binary or base64 with no grep value, and writing them costs more disk than
/// the recovery is worth. Over the cap the truncation notice is emitted without
/// a path.
pub(super) const MAX_SPILL_BYTES: usize = 8 * 1024 * 1024;
/// 单份压缩归档的字节上限。超过时保留**最新**的那部分消息：摘要已经覆盖了更早的
/// 内容，而用户和模型最可能回头查的是最近几轮。
const MAX_ARCHIVE_BYTES: usize = 8 * 1024 * 1024;
/// Readable part of one generated file name. The hash suffix keeps distinct
/// names apart, so the readable halves stay short enough for Windows' MAX_PATH.
const FILE_NAME_PART: usize = 40;

/// Writes the shadowed conversation to a searchable JSONL archive and returns
/// its path.
///
/// The summary slot points at this file, so a model that needs a detail the
/// summary dropped can `Shell`-grep the archive instead of re-reading the
/// repository or asking the user to repeat context.
pub(crate) fn archive_compaction_history(
    conversation_id: &str,
    run_id: &str,
    messages: &[CanonicalMessage],
) -> Option<PathBuf> {
    let directory = config::managed_data_dir()
        .ok()?
        .join(COMPACTION_ARCHIVE_DIR);
    write_archive(&directory, conversation_id, run_id, messages)
}

fn write_archive(
    directory: &Path,
    conversation_id: &str,
    run_id: &str,
    messages: &[CanonicalMessage],
) -> Option<PathBuf> {
    let body = archive_body(messages)?;
    std::fs::create_dir_all(directory).ok()?;
    let stamp = now_millis()?;
    let path = directory.join(archive_file_name(conversation_id, run_id, stamp));
    std::fs::write(&path, body).ok()?;
    Some(path)
}

/// `hex(conversation)-<stamp>-<hash8>.jsonl`
///
/// 会话 id 用十六进制整段编码：维护循环要能从文件名反推出引用，而会话 id 本身
/// 含 `-`，用分隔符解析会在 UUID 上出错。
fn archive_file_name(conversation_id: &str, run_id: &str, stamp: u128) -> String {
    let hash = fnv1a(format!("{conversation_id}\u{0}{run_id}").as_bytes());
    format!(
        "{}-{stamp}-{hash:08x}.jsonl",
        hex_encode(conversation_id.as_bytes())
    )
}

/// Serializes one message per JSONL line, keeping the newest lines that fit.
///
/// 之前的实现遇到超过 8 MB 的历史整份放弃，最大的会话（最需要归档的那种）反而
/// 什么都留不下。现在从最新往回收集，装不下就停——摘要已经覆盖更早的内容。
fn archive_body(messages: &[CanonicalMessage]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut bytes: usize = 0;
    for message in messages.iter().rev() {
        let line = serde_json::to_string(message).ok()?;
        let needed = line.len().saturating_add(1);
        if bytes.saturating_add(needed) > MAX_ARCHIVE_BYTES {
            break;
        }
        bytes += needed;
        lines.push(line);
    }
    if lines.is_empty() {
        // 最新的单条消息本身就超过上限：没有可用的归档。
        return None;
    }
    lines.reverse();
    let mut body = String::with_capacity(bytes);
    for line in lines {
        body.push_str(&line);
        body.push('\n');
    }
    Some(body)
}

/// Best-effort spill of one full tool result. Returns the path only when the
/// bytes were actually written.
pub(super) fn spill_tool_result(call_id: &str, name: &str, content: &str) -> Option<PathBuf> {
    let directory = config::managed_data_dir().ok()?.join(SPILL_DIR);
    write_spill(&directory, call_id, name, content)
}

fn write_spill(directory: &Path, call_id: &str, name: &str, content: &str) -> Option<PathBuf> {
    if content.len() > MAX_SPILL_BYTES {
        return None;
    }
    std::fs::create_dir_all(directory).ok()?;
    let path = directory.join(spill_file_name(name, call_id));
    std::fs::write(&path, content).ok()?;
    Some(path)
}

/// `<hash8>-<hex(call id)>-<readable name>.txt`.
///
/// 调用 id 用十六进制整段编码，既能被维护循环反解出来查引用，又不可能含路径分隔符
/// 或保留名。可读部分截到 40 字符，哈希后缀区分共享前缀的长 id。
fn spill_file_name(name: &str, call_id: &str) -> String {
    let hash = fnv1a(format!("{name}\u{0}{call_id}").as_bytes());
    format!(
        "{hash:08x}-{}-{}.txt",
        hex_encode(call_id.as_bytes()),
        sanitize(name)
    )
}

/// The `call_id` a spill file was written for, when the name can be decoded.
pub(crate) fn spill_call_id(file_name: &str) -> Option<String> {
    let rest = file_name.strip_suffix(".txt")?;
    let (hash, rest) = rest.split_once('-')?;
    if hash.len() != 8 || !hash.chars().all(|character| character.is_ascii_hexdigit()) {
        return None;
    }
    let (encoded, _) = rest.split_once('-')?;
    String::from_utf8(hex_decode(encoded)?).ok()
}

/// The `conversation_id` an archive was written for, when the name decodes.
pub(crate) fn archive_conversation_id(file_name: &str) -> Option<String> {
    let rest = file_name.strip_suffix(".jsonl")?;
    let (encoded, _) = rest.split_once('-')?;
    String::from_utf8(hex_decode(encoded)?).ok()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        encoded.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    encoded
}

fn hex_decode(encoded: &str) -> Option<Vec<u8>> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    let digits = encoded.as_bytes();
    let (pairs, remainder) = digits.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    let mut bytes = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        bytes.push(u8::try_from(high * 16 + low).ok()?);
    }
    Some(bytes)
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn now_millis() -> Option<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_millis())
}

/// 删除不再被引用的落盘文件，返回删掉的数量。
///
/// `live_calls` 是 `tool_round_calls.call_id` 的当前集合，`live_conversations` 是
/// 会话表。文件按名字反解出引用；引用还在就留着（截断提示和摘要槽还指向它）。
pub(crate) fn prune_artifact_files(
    live_calls: &HashSet<String>,
    live_conversations: &HashSet<String>,
) -> usize {
    let Ok(root) = config::managed_data_dir() else {
        return 0;
    };
    prune_artifacts_in(
        &root.join(SPILL_DIR),
        &root.join(COMPACTION_ARCHIVE_DIR),
        live_calls,
        live_conversations,
    )
}

/// 删除已经失去记录的压缩归档，返回删掉的文件数。
///
/// 只删受管目录里的文件：`compaction_events.archive_path` 是库里的文本，不能让它变成
/// 任意路径删除。目录与写入时是同一个 `managed_data_dir()`。
pub(crate) fn remove_compaction_archives(paths: &[String]) -> usize {
    let Ok(root) = config::managed_data_dir() else {
        return 0;
    };
    let Ok(directory) = root.join(COMPACTION_ARCHIVE_DIR).canonicalize() else {
        return 0;
    };
    remove_archives_in(&directory, paths)
}

fn remove_archives_in(directory: &Path, paths: &[String]) -> usize {
    let mut removed = 0;
    for path in paths {
        let path = Path::new(path);
        // 父目录必须就是归档目录；`..` 之类的越界路径在 canonicalize 后不会相等。
        let inside = path
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .is_some_and(|parent| parent == directory);
        if inside && std::fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 维护循环调用的实现；目录可注入，便于测试。
pub(crate) fn prune_artifacts_in(
    spill_directory: &Path,
    archive_directory: &Path,
    live_calls: &HashSet<String>,
    live_conversations: &HashSet<String>,
) -> usize {
    let now = SystemTime::now();
    let spills = prune_directory(spill_directory, SPILL_MAX_FILES, now, |name| {
        spill_call_id(name).map(|call_id| live_calls.contains(&call_id))
    });
    let archives = prune_directory(archive_directory, ARCHIVE_MAX_FILES, now, |name| {
        archive_conversation_id(name)
            .map(|conversation_id| live_conversations.contains(&conversation_id))
    });
    spills + archives
}

/// 一个目录的清理：先删"引用已消失且过了宽限期"的文件，再按数量与字节上限从最老的删。
///
/// 每次维护都扫一遍（不再有"目录够大才扫"的门槛）：扫描搬到维护循环之后，几百上千个
/// `stat` 一分钟一次可以忽略；门槛留着反而会让闲置目录里的死文件永远没人收拾。
fn prune_directory(
    directory: &Path,
    ceiling: usize,
    now: SystemTime,
    reference: impl Fn(&str) -> Option<bool>,
) -> usize {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let age = now.duration_since(modified).unwrap_or_default();
        let name = entry.file_name().to_string_lossy().into_owned();
        // 引用还在：一直留（提示还指向它）。引用没了：过了宽限期就删，不等 7 天。
        // 解析不出引用（旧命名）：退回 7 天窗口。
        let keep = match reference(&name) {
            Some(true) => true,
            Some(false) => age <= GRACE,
            None => age <= RETENTION,
        };
        files.push((modified, metadata.len(), entry.path(), keep));
    }
    let mut removed = 0;
    files.retain(|(_, _, path, keep)| {
        if *keep {
            return true;
        }
        if std::fs::remove_file(path).is_ok() {
            removed += 1;
        }
        false
    });
    let mut bytes: u64 = files.iter().map(|(_, len, _, _)| *len).sum();
    if files.len() > ceiling || bytes > MAX_DIRECTORY_BYTES {
        files.sort_by_key(|(modified, _, _, _)| *modified);
        while !files.is_empty() && (files.len() > ceiling || bytes > MAX_DIRECTORY_BYTES) {
            let (_, len, path, _) = files.remove(0);
            if std::fs::remove_file(&path).is_ok() {
                bytes = bytes.saturating_sub(len);
                removed += 1;
            }
        }
    }
    removed
}

fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(FILE_NAME_PART)
        .collect();
    if cleaned.is_empty() {
        "result".into()
    } else {
        cleaned
    }
}

/// Test seam: write into an explicit directory instead of the managed one.
#[cfg(test)]
pub(crate) fn write_spill_for_test(
    directory: &Path,
    call_id: &str,
    name: &str,
    content: &str,
) -> Option<PathBuf> {
    write_spill(directory, call_id, name, content)
}

/// Test seam: archive into an explicit directory instead of the managed one.
#[cfg(test)]
pub(crate) fn write_archive_for_test(
    directory: &Path,
    conversation_id: &str,
    run_id: &str,
    messages: &[CanonicalMessage],
) -> Option<PathBuf> {
    write_archive(directory, conversation_id, run_id, messages)
}

/// Test seam: prune one directory with a small ceiling and an explicit reference set.
#[cfg(test)]
pub(crate) fn prune_directory_for_test(
    directory: &Path,
    ceiling: usize,
    reference: impl Fn(&str) -> Option<bool>,
) -> usize {
    prune_directory(directory, ceiling, SystemTime::now(), reference)
}

#[cfg(test)]
pub(crate) fn prune_artifacts_for_test(
    spill_directory: &Path,
    archive_directory: &Path,
    live_calls: &HashSet<String>,
    live_conversations: &HashSet<String>,
) -> usize {
    prune_artifacts_in(
        spill_directory,
        archive_directory,
        live_calls,
        live_conversations,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageContent, Origin, Role, ToolResultContent};

    fn message(text: &str) -> CanonicalMessage {
        CanonicalMessage::text("message-1", Role::User, Origin::User, text)
    }

    fn write_with_modified(path: &Path, seconds: u64) {
        std::fs::write(path, "").unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
            .unwrap();
    }

    fn now_seconds() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn calls(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn spill_writes_the_full_content_and_records_the_call_id() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_spill_for_test(
            directory.path(),
            "call/with:separators",
            "Read",
            "the full tool output",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "the full tool output"
        );
        let file_name = path.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(
            spill_call_id(&file_name).as_deref(),
            Some("call/with:separators"),
            "the maintenance loop must be able to resolve the reference"
        );
        assert!(file_name.ends_with("-Read.txt"), "{file_name}");
    }

    #[test]
    fn spill_names_stay_short_and_distinguish_long_ids_sharing_a_prefix() {
        let long_id = "a".repeat(500);
        let first = spill_file_name("Read", &long_id);
        let second = spill_file_name("Read", &format!("{long_id}b"));
        assert_eq!(first, spill_file_name("Read", &long_id), "deterministic");
        assert_ne!(first, second, "a shared readable prefix must not collide");
        assert!(
            first.len() <= FILE_NAME_PART + 16 + 1 + long_id.len() * 2,
            "file name must stay bounded: {first}"
        );
        assert!(
            first
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || matches!(character, '-' | '_' | '.')),
            "no traversal or reserved-name characters: {first}"
        );
    }

    #[test]
    fn oversized_tool_results_are_not_spilled() {
        let directory = tempfile::tempdir().unwrap();
        let too_big = "x".repeat(MAX_SPILL_BYTES + 1);
        assert!(
            write_spill_for_test(directory.path(), "call-1", "Read", &too_big).is_none(),
            "over the cap the caller falls back to a notice without a path"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn archive_writes_one_json_line_per_message() {
        let directory = tempfile::tempdir().unwrap();
        let messages = vec![
            CanonicalMessage::text("m1", Role::User, Origin::User, "hello"),
            CanonicalMessage::text("m2", Role::Assistant, Origin::Assistant, "hi"),
        ];

        let path =
            write_archive_for_test(directory.path(), "conversation-1", "run-1", &messages).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one line per message");
        for (line, expected) in lines.iter().zip(messages.iter()) {
            let decoded: CanonicalMessage = serde_json::from_str(line).unwrap();
            assert_eq!(&decoded, expected);
        }
        let file_name = path.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(
            archive_conversation_id(&file_name).as_deref(),
            Some("conversation-1")
        );
    }

    /// 超过上限时保留最新的消息，而不是整份放弃：最大的会话最需要归档。
    #[test]
    fn oversized_archives_keep_the_newest_messages() {
        let directory = tempfile::tempdir().unwrap();
        let old = vec![message(&"x".repeat(MAX_ARCHIVE_BYTES - 100))];
        let newest = message("the newest turn");
        let mut messages = old;
        messages.push(newest.clone());

        let path =
            write_archive_for_test(directory.path(), "conversation-1", "run-1", &messages).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 1, "only the newest line fits");
        let decoded: CanonicalMessage = serde_json::from_str(body.trim_end()).unwrap();
        assert_eq!(decoded, newest);
    }

    /// 删除孤儿归档只作用于受管归档目录：`archive_path` 是库里的文本，不能变成
    /// 任意路径删除。
    #[test]
    fn orphan_archive_removal_is_confined_to_the_archive_directory() {
        let archive = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let directory = archive.path().canonicalize().unwrap();
        let inside = directory.join("orphan.jsonl");
        std::fs::write(&inside, b"gone").unwrap();
        let kept = outside.path().join("keep.jsonl");
        std::fs::write(&kept, b"keep").unwrap();

        let removed = remove_archives_in(
            &directory,
            &[inside.display().to_string(), kept.display().to_string()],
        );

        assert_eq!(removed, 1);
        assert!(!inside.exists());
        assert!(kept.exists(), "受管目录之外的文件一个都不能碰");
    }

    /// 引用还在的文件不受天数限制：这正是"提示里的路径 7 天后失效"的修复。
    #[test]
    fn files_with_a_live_reference_survive_past_the_retention_window() {
        let spill = tempfile::tempdir().unwrap();
        let archive = tempfile::tempdir().unwrap();
        let now = now_seconds();
        let live = spill.path().join(spill_file_name("Read", "call-live"));
        write_with_modified(&live, now - 30 * 24 * 60 * 60);
        let archive_path = archive.path().join(archive_file_name(
            "conversation-live",
            "run-1",
            1_760_000_000_000,
        ));
        write_with_modified(&archive_path, now - 30 * 24 * 60 * 60);

        prune_artifacts_for_test(
            spill.path(),
            archive.path(),
            &calls(&["call-live"]),
            &calls(&["conversation-live"]),
        );

        assert!(live.exists(), "a referenced spill is never expired by age");
        assert!(
            archive_path.exists(),
            "a referenced archive is never expired by age"
        );
    }

    /// 引用没了（会话被回收）且过了宽限期：立刻删，不必等 7 天。
    #[test]
    fn files_without_a_reference_are_removed_after_the_grace_period() {
        let spill = tempfile::tempdir().unwrap();
        let archive = tempfile::tempdir().unwrap();
        let now = now_seconds();
        let dead = spill.path().join(spill_file_name("Read", "call-dead"));
        write_with_modified(&dead, now - 2 * 60 * 60);
        let fresh = spill.path().join(spill_file_name("Read", "call-fresh"));
        write_with_modified(&fresh, now);

        prune_artifacts_for_test(spill.path(), archive.path(), &calls(&[]), &calls(&[]));

        assert!(
            !dead.exists(),
            "an unreferenced file past the grace period is gone"
        );
        assert!(
            fresh.exists(),
            "the grace period protects a just-written file"
        );
    }

    /// 全都还有引用时，数量上限仍然生效（磁盘有界优先）。
    #[test]
    fn the_ceiling_still_bounds_a_directory_full_of_live_files() {
        let spill = tempfile::tempdir().unwrap();
        let now = now_seconds();
        let total = 6_u64;
        for index in 0..total {
            let path = spill
                .path()
                .join(spill_file_name("Read", &format!("call-index-{index:04}")));
            write_with_modified(&path, now - (total - index));
        }
        let live: HashSet<String> = (0..total)
            .map(|index| format!("call-index-{index:04}"))
            .collect();
        prune_directory_for_test(spill.path(), 3, |name| {
            spill_call_id(name).map(|call_id| live.contains(&call_id))
        });
        assert_eq!(
            std::fs::read_dir(spill.path()).unwrap().count(),
            3,
            "the ceiling bounds the directory even when every file is referenced"
        );
    }

    /// 解析不出引用的旧文件按 7 天窗口清理，不会永远留着。
    #[test]
    fn legacy_names_fall_back_to_the_retention_window() {
        let spill = tempfile::tempdir().unwrap();
        let archive = tempfile::tempdir().unwrap();
        let now = now_seconds();
        let legacy = spill.path().join("Read-call_legacy-0123abcd.txt");
        write_with_modified(&legacy, now - 8 * 24 * 60 * 60);

        prune_artifacts_for_test(spill.path(), archive.path(), &calls(&[]), &calls(&[]));

        assert!(!legacy.exists(), "an unparseable file still expires by age");
    }

    #[test]
    fn archive_lines_keep_message_content_serializable() {
        let directory = tempfile::tempdir().unwrap();
        let messages = vec![CanonicalMessage {
            message_id: "tool-1".into(),
            role: Role::Tool,
            origin: Origin::Tool,
            content: MessageContent::ToolResult(ToolResultContent {
                call_id: "call-1".into(),
                name: "Read".into(),
                content: "file\ncontent".into(),
                is_error: false,
                image: None,
                provider_parts: Vec::new(),
            }),
            runtime_event_id: None,
        }];

        let path =
            write_archive_for_test(directory.path(), "conversation-2", "run-2", &messages).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 1);
        let decoded: CanonicalMessage = serde_json::from_str(body.trim_end()).unwrap();
        assert_eq!(decoded, messages[0]);
    }
}
