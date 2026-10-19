//! Spills full tool results that the model-visible size gate truncated, and
//! archives the conversation a compaction shadowed.
//!
//! Both artifacts are best effort: a failed write, prune, or size-cap decision
//! must never fail a tool result or a run. Files live in the managed data
//! directory and are pruned lazily after writes: files past the retention
//! window first, then the oldest excess once a hard count ceiling is crossed.
//! No background task owns their lifetime.
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::config;

use super::CanonicalMessage;

const SPILL_DIR: &str = "tool-results";
const COMPACTION_ARCHIVE_DIR: &str = "compaction-history";
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Directories are only scanned once they grow past these counts: a `read_dir`
/// on every write would add IO to the tool hot path for nothing.
const SPILL_PRUNE_THRESHOLD: usize = 256;
const ARCHIVE_PRUNE_THRESHOLD: usize = 64;
/// Hard file-count ceilings enforced once a directory is scanned. The
/// retention window alone is not a disk bound: a heavy week can leave gigabytes
/// behind. Only the newest archive/spill paths can appear in live provider
/// history, so the oldest excess beyond these counts is safe to delete even
/// inside the retention window.
const SPILL_MAX_FILES: usize = 2_048;
const ARCHIVE_MAX_FILES: usize = 256;
// A ceiling below the scan threshold would never be enforced.
const _: () = assert!(SPILL_MAX_FILES > SPILL_PRUNE_THRESHOLD);
const _: () = assert!(ARCHIVE_MAX_FILES > ARCHIVE_PRUNE_THRESHOLD);
/// Largest single tool result worth spilling. Bigger payloads are usually
/// binary or base64 with no grep value, and writing them costs more disk than
/// the recovery is worth. Over the cap the truncation notice is emitted without
/// a path.
pub(super) const MAX_SPILL_BYTES: usize = 8 * 1024 * 1024;
/// Largest compaction archive. The archive is a recovery hint, not a backup; a
/// multi-megabyte archive is mostly redundant history and is skipped instead.
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
    run_id: &str,
    messages: &[CanonicalMessage],
) -> Option<PathBuf> {
    let directory = config::managed_data_dir()
        .ok()?
        .join(COMPACTION_ARCHIVE_DIR);
    write_archive(&directory, run_id, messages)
}

fn write_archive(directory: &Path, run_id: &str, messages: &[CanonicalMessage]) -> Option<PathBuf> {
    let body = archive_body(messages)?;
    std::fs::create_dir_all(directory).ok()?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    let path = directory.join(format!("{}-{stamp}.jsonl", sanitize(run_id)));
    std::fs::write(&path, body).ok()?;
    prune_dir(directory, ARCHIVE_PRUNE_THRESHOLD, ARCHIVE_MAX_FILES);
    Some(path)
}

/// Serializes one message per JSONL line, or `None` when the archive would
/// exceed the cap.
fn archive_body(messages: &[CanonicalMessage]) -> Option<String> {
    let mut body = String::new();
    for message in messages {
        let line = serde_json::to_string(message).ok()?;
        if body.len().saturating_add(line.len()).saturating_add(1) > MAX_ARCHIVE_BYTES {
            return None;
        }
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
    prune_dir(directory, SPILL_PRUNE_THRESHOLD, SPILL_MAX_FILES);
    Some(path)
}

/// `<readable name>-<readable id>-<hash>.txt`.
///
/// The FNV-1a suffix hashes the full name and id, so two long ids that share a
/// readable prefix can never overwrite each other. Everything outside
/// `[A-Za-z0-9_-]` becomes `_`, which also rules out path traversal and
/// reserved names.
fn spill_file_name(name: &str, call_id: &str) -> String {
    let hash = fnv1a(format!("{name}\u{0}{call_id}").as_bytes());
    format!("{}-{}-{hash:08x}.txt", sanitize(name), sanitize(call_id))
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Enforces the retention window and the hard file-count ceiling once a
/// directory is larger than `threshold`.
///
/// Stale files go first; then, if the directory is still above `ceiling`, the
/// oldest remaining files go too so a heavy week cannot grow the directory
/// without bound. Every failure is ignored: cleanup must not fail the tool
/// result and the next write retries.
fn prune_dir(directory: &Path, threshold: usize, ceiling: usize) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let entries: Vec<_> = entries.flatten().collect();
    if entries.len() <= threshold {
        return;
    }
    let now = SystemTime::now();
    let mut kept = Vec::new();
    for entry in entries {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() > RETENTION {
            let _ = std::fs::remove_file(entry.path());
        } else {
            kept.push((modified, entry.path()));
        }
    }
    if kept.len() <= ceiling {
        return;
    }
    let excess = kept.len() - ceiling;
    kept.sort_by_key(|(modified, _)| *modified);
    for (_, path) in kept.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
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
    run_id: &str,
    messages: &[CanonicalMessage],
) -> Option<PathBuf> {
    write_archive(directory, run_id, messages)
}

#[cfg(test)]
pub(crate) fn prune_spills_for_test(directory: &Path) {
    prune_dir(directory, SPILL_PRUNE_THRESHOLD, SPILL_MAX_FILES);
}

#[cfg(test)]
pub(crate) fn prune_archives_for_test(directory: &Path) {
    prune_dir(directory, ARCHIVE_PRUNE_THRESHOLD, ARCHIVE_MAX_FILES);
}

/// Test seam for the retention/ceiling logic with small counts.
#[cfg(test)]
pub(crate) fn prune_dir_for_test(directory: &Path, threshold: usize, ceiling: usize) {
    prune_dir(directory, threshold, ceiling);
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

    #[test]
    fn spill_writes_the_full_content_and_sanitizes_the_name() {
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
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            spill_file_name("Read", "call/with:separators")
        );
        let file_name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            file_name.starts_with("Read-call_with_separators-"),
            "{file_name}"
        );
    }

    #[test]
    fn spill_names_stay_short_and_distinguish_long_ids_sharing_a_prefix() {
        let long_id = "a".repeat(500);
        let first = spill_file_name("Read", &long_id);
        let second = spill_file_name("Read", &format!("{long_id}b"));
        assert_eq!(first, spill_file_name("Read", &long_id), "deterministic");
        assert_ne!(first, second, "a shared readable prefix must not collide");
        assert!(
            first.len() <= FILE_NAME_PART * 2 + 14,
            "file name must stay short: {first}"
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

        let path = write_archive_for_test(directory.path(), "run-1", &messages).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one line per message");
        for (line, expected) in lines.iter().zip(messages.iter()) {
            let decoded: CanonicalMessage = serde_json::from_str(line).unwrap();
            assert_eq!(&decoded, expected);
        }
    }

    #[test]
    fn oversized_archives_are_skipped() {
        let directory = tempfile::tempdir().unwrap();
        let messages = vec![message(&"x".repeat(MAX_ARCHIVE_BYTES + 1))];
        assert!(write_archive_for_test(directory.path(), "run-1", &messages).is_none());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn archive_prune_removes_stale_files_and_keeps_fresh_ones() {
        let directory = tempfile::tempdir().unwrap();
        let now = now_seconds();
        let stale = directory.path().join("stale.jsonl");
        write_with_modified(&stale, now - 8 * 24 * 60 * 60);
        for index in 0..ARCHIVE_PRUNE_THRESHOLD {
            std::fs::write(directory.path().join(format!("fresh-{index}.jsonl")), "").unwrap();
        }

        prune_archives_for_test(directory.path());

        assert!(!stale.exists(), "stale archives are removed");
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            ARCHIVE_PRUNE_THRESHOLD,
            "fresh archives are kept"
        );
    }

    #[test]
    fn spill_prune_waits_for_enough_files_and_removes_only_stale_ones() {
        let directory = tempfile::tempdir().unwrap();
        let now = now_seconds();
        let stale = directory.path().join("stale.txt");
        write_with_modified(&stale, now - 8 * 24 * 60 * 60);

        // Below the threshold nothing is scanned, so even a stale file stays.
        prune_spills_for_test(directory.path());
        assert!(stale.exists(), "prune is skipped for a small directory");

        for index in 0..SPILL_PRUNE_THRESHOLD {
            std::fs::write(directory.path().join(format!("fresh-{index}.txt")), "").unwrap();
        }
        prune_spills_for_test(directory.path());

        assert!(!stale.exists(), "stale spills are removed");
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            SPILL_PRUNE_THRESHOLD
        );
    }

    /// 目录超过上限时,即使文件都还年轻,也要从最老的开始删到上限以内:
    /// 只按 7 天保留期删,重度使用一周仍可能攒到 GB 级。
    #[test]
    fn prune_enforces_the_hard_ceiling_after_the_retention_window() {
        let directory = tempfile::tempdir().unwrap();
        let now = now_seconds();
        // f0 is the oldest, f5 the newest; all are inside the retention window.
        for index in 0..6_u64 {
            write_with_modified(
                &directory.path().join(format!("f{index}.txt")),
                now - (60 - index),
            );
        }

        prune_dir_for_test(directory.path(), 2, 3);

        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            3,
            "the ceiling bounds the directory"
        );
        for index in 0..3 {
            assert!(
                !directory.path().join(format!("f{index}.txt")).exists(),
                "the oldest file f{index} is dropped first"
            );
        }
        for index in 3..6 {
            assert!(
                directory.path().join(format!("f{index}.txt")).exists(),
                "the newest file f{index} survives"
            );
        }
    }

    #[test]
    fn prune_removes_stale_files_before_the_ceiling() {
        let directory = tempfile::tempdir().unwrap();
        let now = now_seconds();
        for index in 0..2_u64 {
            write_with_modified(
                &directory.path().join(format!("stale{index}.txt")),
                now - 8 * 24 * 60 * 60 - index,
            );
        }
        for index in 0..4_u64 {
            write_with_modified(
                &directory.path().join(format!("fresh{index}.txt")),
                now - (60 - index),
            );
        }

        prune_dir_for_test(directory.path(), 2, 3);

        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 3);
        // Stale files go first; then the oldest fresh file reaches the ceiling.
        for name in ["stale0.txt", "stale1.txt", "fresh0.txt"] {
            assert!(
                !directory.path().join(name).exists(),
                "{name} should be gone"
            );
        }
        for name in ["fresh1.txt", "fresh2.txt", "fresh3.txt"] {
            assert!(directory.path().join(name).exists(), "{name} should stay");
        }
    }

    /// 归档目录的真实接线:超过 ARCHIVE_MAX_FILES 后删到上限以内。
    #[test]
    fn archive_prune_caps_the_directory_at_the_ceiling() {
        let directory = tempfile::tempdir().unwrap();
        let now = now_seconds();
        let total = ARCHIVE_MAX_FILES + 10;
        for index in 0..total {
            write_with_modified(
                &directory.path().join(format!("archive-{index:04}.jsonl")),
                now - (total - index) as u64,
            );
        }

        prune_archives_for_test(directory.path());

        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            ARCHIVE_MAX_FILES,
            "the archive directory is capped"
        );
        for index in 0..10 {
            assert!(
                !directory
                    .path()
                    .join(format!("archive-{index:04}.jsonl"))
                    .exists(),
                "the 10 oldest archives are dropped"
            );
        }
        assert!(directory
            .path()
            .join(format!("archive-{:04}.jsonl", total - 1))
            .exists());
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

        let path = write_archive_for_test(directory.path(), "run-2", &messages).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 1);
        let decoded: CanonicalMessage = serde_json::from_str(body.trim_end()).unwrap();
        assert_eq!(decoded, messages[0]);
    }
}
