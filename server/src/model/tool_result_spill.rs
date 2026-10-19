//! Spills full tool results that the model-visible size gate truncated.
//!
//! The size gate keeps provider history bounded, but it used to discard the
//! omitted middle permanently. The full rendered result is written to the
//! managed data directory instead; the truncated provider copy carries its
//! absolute path, so the model can `Shell`-read the missing part or grep it
//! instead of re-running the tool.
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::{config, Result};

const SPILL_DIR: &str = "tool-results";
const SPILL_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const SPILL_PRUNE_THRESHOLD: usize = 1_024;

/// Best-effort spill of one full tool result. Returns the path only when the
/// bytes were actually written.
pub(crate) fn spill_tool_result(call_id: &str, name: &str, content: &str) -> Option<PathBuf> {
    let directory = config::managed_data_dir().ok()?.join(SPILL_DIR);
    write_spill(&directory, call_id, name, content).ok()
}

fn write_spill(directory: &Path, call_id: &str, name: &str, content: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join(format!("{}-{}.txt", sanitize(name), sanitize(call_id)));
    std::fs::write(&path, content)?;
    prune_spills(directory);
    Ok(path)
}

/// Removes stale spill files once the directory grows past a small threshold.
/// Every write best-effort: a failed cleanup must not fail the tool result.
fn prune_spills(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let entries: Vec<_> = entries.flatten().collect();
    if entries.len() <= SPILL_PRUNE_THRESHOLD {
        return;
    }
    let now = SystemTime::now();
    for entry in entries {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() > SPILL_RETENTION {
            let _ = std::fs::remove_file(entry.path());
        }
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
        .take(96)
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
) -> Result<PathBuf> {
    write_spill(directory, call_id, name, content)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "Read-call_with_separators.txt"
        );
    }
}
