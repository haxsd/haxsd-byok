//! Applies the model-visible size gate to Tool results.
//!
//! Tool-specific middle truncation (Shell streams, GenerateImage base64, Edit
//! diffs) runs before the total byte limit. The full pre-compaction result is
//! spilled only when the gate actually omitted bytes the model might need back,
//! and the truncation notice then points at that copy.

use std::path::{Path, PathBuf};

use serde_json::Value;

const KIB: usize = 1024;

/// Applies the tool-specific compaction and the total size limit for one result.
///
/// `call_id` names the spilled file when the gate omits bytes. It is `None`
/// only where the managed data directory must not be touched (tests).
pub(crate) fn limit_tool_result_text(name: &str, content: &str, call_id: Option<&str>) -> String {
    limit_tool_result_text_with(name, content, |raw| {
        call_id.and_then(|call_id| super::tool_result_spill::spill_tool_result(call_id, name, raw))
    })
}

/// Test variant that spills into an explicit directory.
#[cfg(test)]
pub(crate) fn limit_tool_result_text_in_dir(
    name: &str,
    content: &str,
    call_id: &str,
    directory: &Path,
) -> String {
    limit_tool_result_text_with(name, content, |raw| {
        super::tool_result_spill::write_spill_for_test(directory, call_id, name, raw)
    })
}

fn limit_tool_result_text_with(
    name: &str,
    content: &str,
    spill: impl FnOnce(&str) -> Option<PathBuf>,
) -> String {
    let Some(limit) = replay_limit(name) else {
        return content.to_string();
    };
    let compacted = compact_for_replay(name, content).unwrap_or_else(|| content.to_string());
    if compacted.len() <= limit {
        // The tool-specific compaction removed bytes without crossing the total
        // limit (Shell stream middles are the common case). Point at the full
        // copy so the omitted middle stays discoverable, but never exceed the
        // limit just to add the notice.
        if compacted == content || !may_spill(name) {
            return compacted;
        }
        let Some(path) = spill(content) else {
            return compacted;
        };
        let notice = format!("\n\n[full result stored at: {}]", path.display());
        if compacted.len().saturating_add(notice.len()) <= limit {
            return format!("{compacted}{notice}");
        }
        // The notice would push the result over the limit: keep the bounded
        // copy and do not leave a file no notice points at.
        let _ = std::fs::remove_file(&path);
        return compacted;
    }
    // Bytes are actually omitted: spill the raw result and name it in the
    // truncation notice. A missing spill (write failure or size cap) degrades
    // to the old notice without a path.
    let spill = may_spill(name).then(|| spill(content)).flatten();
    truncate_replay_text(name, &compacted, limit, spill.as_deref())
}

/// Base64 image payloads have no grep value; spilling them would only write
/// megabytes of text the model cannot search.
fn may_spill(name: &str) -> bool {
    name.trim() != "GenerateImage"
}

/// The gate's size limit for one canonical tool name, if it has one.
fn replay_limit(name: &str) -> Option<usize> {
    match name.trim() {
        "GenerateImage" | "WebSearch" => Some(16 * KIB),
        "Read" => Some(64 * KIB),
        "Shell" => Some(128 * KIB),
        "Grep" | "Glob" => Some(32 * KIB),
        "PatchEdit" | "PatchEditLines" | "PatchEditSpan" | "StrReplace" => Some(4 * KIB),
        "Edit" | "EditNotebook" | "Write" | "WebFetch" => Some(32 * KIB),
        "CallMcpTool" | "FetchMcpResource" | "ListMcpResources" | "GetMcpTools"
        | "SembleSearch" | "SembleFindRelated" => Some(32 * KIB),
        _ => None,
    }
}

/// Tool-specific compaction that runs before the total limit.
fn compact_for_replay(name: &str, content: &str) -> Option<String> {
    match name.trim() {
        "GenerateImage" => compact_generate_image(content),
        "Shell" => compact_shell(content),
        "PatchEdit" | "PatchEditLines" | "PatchEditSpan" | "StrReplace" | "Edit" | "Write" => {
            compact_edit(name, content)
        }
        _ => None,
    }
}

fn truncate_replay_text(name: &str, content: &str, limit: usize, spill: Option<&Path>) -> String {
    if content.len() <= limit {
        return content.to_string();
    }
    let original = content.len();
    let mut shown = limit;
    loop {
        let notice = match spill {
            Some(path) => format!(
                "\n\n[truncated: {name} result exceeded {limit} bytes; showing {shown} of {original} bytes; full result stored at: {}]",
                path.display()
            ),
            None => format!(
                "\n\n[truncated: {name} result exceeded {limit} bytes; showing {shown} of {original} bytes]"
            ),
        };
        if notice.len() >= limit {
            // The path notice alone is already over a very small budget; keep
            // the notice so the full result remains discoverable.
            return match spill {
                Some(_) => notice.trim_start_matches('\n').to_string(),
                None => utf8_prefix(content, limit).to_string(),
            };
        }
        let available = limit.saturating_sub(notice.len());
        let kept = utf8_prefix(content, available);
        if kept.len() == shown {
            return format!("{}{notice}", kept.trim_end_matches('\n'));
        }
        shown = kept.len();
    }
}

fn compact_generate_image(content: &str) -> Option<String> {
    let mut value = serde_json::from_str::<Value>(content.trim()).ok()?;
    if !replace_image_data(&mut value) {
        return None;
    }
    serde_json::to_string(&value).ok()
}

fn replace_image_data(value: &mut Value) -> bool {
    match value {
        Value::Object(object) => {
            let mut changed = false;
            for (key, child) in object.iter_mut() {
                if matches!(key.as_str(), "image_data" | "imageData") {
                    if let Value::String(data) = child {
                        if data.starts_with("[base64 image data omitted from replay; bytes=") {
                            continue;
                        }
                        *child = Value::String(format!(
                            "[base64 image data omitted from replay; bytes={}]",
                            data.trim().len()
                        ));
                        changed = true;
                        continue;
                    }
                }
                changed |= replace_image_data(child);
            }
            changed
        }
        Value::Array(items) => items.iter_mut().any(replace_image_data),
        _ => false,
    }
}

fn compact_shell(content: &str) -> Option<String> {
    let mut value = serde_json::from_str::<Value>(content.trim()).ok()?;
    if !compact_shell_fields(&mut value) {
        return None;
    }
    serde_json::to_string(&value).ok()
}

fn compact_shell_fields(value: &mut Value) -> bool {
    match value {
        Value::Object(object) => {
            let mut changed = false;
            for (key, child) in object.iter_mut() {
                if let Value::String(text) = child {
                    let limit = match key.as_str() {
                        "stdout" | "stderr" => Some(16 * KIB),
                        "interleaved_output" | "interleavedOutput" => Some(32 * KIB),
                        _ => None,
                    };
                    if let Some(limit) = limit {
                        let next = truncate_middle(&format!("Shell {key}"), text, limit);
                        if next != *text {
                            *text = next;
                            changed = true;
                        }
                        continue;
                    }
                }
                changed |= compact_shell_fields(child);
            }
            changed
        }
        Value::Array(items) => items.iter_mut().any(compact_shell_fields),
        _ => false,
    }
}

fn compact_edit(name: &str, content: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(content.trim()).ok()?;
    let success = value.get("success")?.as_object()?;
    let diff = success
        .get("diff_string")
        .or_else(|| success.get("diffString"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| truncate_replay_text(name, text, edit_limit(name), None));
    if let Some(diff) = diff {
        return Some(serde_json::json!({"success": {"diff_string": diff}}).to_string());
    }
    let after = success
        .get("after_full_file_content")
        .or_else(|| success.get("afterFullFileContent"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| truncate_replay_text(name, text, edit_limit(name), None));
    after
        .map(|after| serde_json::json!({"success": {"after_full_file_content": after}}).to_string())
}

fn edit_limit(name: &str) -> usize {
    match name.trim() {
        "PatchEdit" | "PatchEditLines" | "PatchEditSpan" | "StrReplace" => 4 * KIB,
        _ => 32 * KIB,
    }
}

fn truncate_middle(name: &str, content: &str, limit: usize) -> String {
    if content.len() <= limit {
        return content.to_string();
    }
    let original = content.len();
    let mut shown = limit;
    loop {
        let notice = format!(
            "\n\n[truncated: {name} result exceeded {limit} bytes; omitted middle; showing {shown} of {original} bytes]\n\n"
        );
        let available = limit.saturating_sub(notice.len());
        let head = utf8_prefix(content, available / 2);
        let tail = utf8_suffix(content, available.saturating_sub(head.len()));
        let next_shown = head.len() + tail.len();
        let next_notice = format!(
            "\n\n[truncated: {name} result exceeded {limit} bytes; omitted middle; showing {next_shown} of {original} bytes]\n\n"
        );
        let output = format!("{head}{next_notice}{tail}");
        if output.len() <= limit || next_notice == notice {
            return output;
        }
        shown = next_shown;
    }
}

fn utf8_prefix(value: &str, limit: usize) -> &str {
    let mut end = limit.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn utf8_suffix(value: &str, limit: usize) -> &str {
    let mut start = value.len().saturating_sub(limit);
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spill_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn spilled_files(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect()
    }

    #[test]
    fn a_canonical_result_without_omissions_is_untouched() {
        let content = "short result";
        assert_eq!(
            limit_tool_result_text("Read", content, Some("call-1")),
            content
        );
        // Tools without a gate limit keep their result verbatim.
        let large = "x".repeat(200 * KIB);
        assert_eq!(
            limit_tool_result_text("TodoWrite", &large, Some("call-1")),
            large
        );
    }

    #[test]
    fn over_limit_results_spill_the_raw_copy_and_name_its_path() {
        let directory = spill_dir();
        let raw = "r".repeat(200 * KIB);

        let content = limit_tool_result_text_in_dir("Read", &raw, "call-1", directory.path());

        assert!(content.len() <= 64 * KIB, "bounded: {}", content.len());
        assert!(content.contains("[truncated: Read"));
        assert!(content.contains("full result stored at"), "{content}");
        let files = spilled_files(directory.path());
        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::read_to_string(&files[0]).unwrap(), raw);
        assert!(content.contains(&files[0].display().to_string()));
    }

    #[test]
    fn shell_middle_truncation_points_at_the_full_result_even_when_it_fits() {
        let directory = spill_dir();
        let raw = serde_json::json!({"stdout": "o".repeat(200 * KIB)}).to_string();

        let content = limit_tool_result_text_in_dir("Shell", &raw, "call-1", directory.path());

        assert!(content.len() <= 128 * KIB, "bounded: {}", content.len());
        assert!(content.contains("omitted middle"), "{content}");
        assert!(content.contains("full result stored at"), "{content}");
        let files = spilled_files(directory.path());
        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::read_to_string(&files[0]).unwrap(), raw);
    }

    #[test]
    fn generate_image_base64_is_never_spilled() {
        let directory = spill_dir();
        let raw = serde_json::json!({"image_data": "A".repeat(64 * KIB)}).to_string();

        let content =
            limit_tool_result_text_in_dir("GenerateImage", &raw, "call-1", directory.path());

        assert!(content.contains("base64 image data omitted from replay"));
        assert!(!content.contains("full result stored at"), "{content}");
        assert!(
            spilled_files(directory.path()).is_empty(),
            "base64 has no grep value"
        );
    }

    #[test]
    fn over_cap_results_fall_back_to_the_notice_without_a_path() {
        let directory = spill_dir();
        let raw = "x".repeat(super::super::tool_result_spill::MAX_SPILL_BYTES + 1);

        let content = limit_tool_result_text_in_dir("Read", &raw, "call-1", directory.path());

        assert!(content.contains("[truncated: Read"));
        assert!(!content.contains("full result stored at"), "{content}");
        assert!(spilled_files(directory.path()).is_empty());
    }

    #[test]
    fn without_a_call_id_no_file_is_written_and_the_old_notice_stays() {
        let directory = spill_dir();
        let raw = "r".repeat(200 * KIB);

        let content = limit_tool_result_text("Read", &raw, None);

        assert!(content.contains("[truncated: Read"));
        assert!(!content.contains("full result stored at"), "{content}");
        assert!(spilled_files(directory.path()).is_empty());
    }
}
