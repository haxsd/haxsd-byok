//! Structure-based access to Devin's own endpoint settings.
//!
//! Devin only reads `codeium.apiServerUrl` and
//! `codeium.inferenceApiServerUrl` when the `Codeium.codeium-dev` extension is
//! present. Packaged builds therefore ignore those user settings and keep using
//! the official endpoints. This module removes that one development-only gate
//! from a uniquely identified endpoint settings function.
//!
//! The patch never embeds gateway URLs: the two user settings remain the source
//! of truth. Surrounding settings code identifies the gate across builds;
//! SHA-256 verifies the backup and restore receipt next to the host file.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const BACKUP_SUFFIX: &str = ".devin-router-gate.backup";
pub const RECEIPT_SUFFIX: &str = ".devin-router-gate.receipt";

const CLEAN_GATE_PATTERN: &str = r"if\(![A-Za-z_$][A-Za-z0-9_$]*\(\)&&!\(0,[A-Za-z_$][A-Za-z0-9_$]*\.isDevelopment\)\(\)\)return ([A-Za-z_$][A-Za-z0-9_$]*)\(\);";
const PATCHED_GATE_PATTERN: &str = r"if\(!1\)return ([A-Za-z_$][A-Za-z0-9_$]*)\(\);";
const GATE_CONTEXT_WINDOW: usize = 2_048;
const GATE_CONTEXT_ANCHORS: [&str; 7] = [
    "deployment_default",
    "getConfiguration",
    "workspace.isTrusted",
    "workspaceFolderValue",
    "workspaceValue",
    "globalValue",
    "user_setting",
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GateStatus {
    pub path: Option<PathBuf>,
    pub version: String,
    pub compatible: bool,
    pub clean: bool,
    pub patched: bool,
    pub backup_path: Option<PathBuf>,
    pub backup_available: bool,
    pub current_sha256: String,
    pub backup_sha256: Option<String>,
    /// Present only while the receipt still describes the current file and a
    /// verified clean backup. Restore never reconstructs a receipt from hashes.
    pub restore_receipt: Option<GateReceipt>,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GateReceipt {
    pub path: PathBuf,
    pub backup_path: PathBuf,
    pub version: String,
    pub original_sha256: String,
    pub patched_sha256: String,
}

pub fn inspect(path: Option<&Path>) -> GateStatus {
    path.map_or_else(GateStatus::not_found, inspect_existing)
}

pub fn apply(path: &Path) -> Result<GateReceipt> {
    apply_with(path)
}

pub fn restore(receipt: &GateReceipt) -> Result<()> {
    recover_displaced(&receipt.path);
    let current =
        fs::read(&receipt.path).map_err(|error| host_read_error(&receipt.path, &error))?;
    if sha256(&current) != receipt.patched_sha256 {
        return Err(restore_refusal(
            receipt,
            "Devin extension changed after this application patched it; restore refused",
        ));
    }
    let backup = fs::read(&receipt.backup_path).map_err(|error| {
        Error::Config(format!(
            "Devin extension backup {} is unavailable ({error}); restore refused; restore the \
             clean Devin installation or reinstall Devin; backup: {}",
            receipt.backup_path.display(),
            receipt.backup_path.display()
        ))
    })?;
    if sha256(&backup) != receipt.original_sha256
        || !is_clean_bytes(&backup, &receipt.original_sha256)
    {
        return Err(restore_refusal(
            receipt,
            "Devin extension backup does not match the verified clean file; restore refused",
        ));
    }
    atomic_replace(&receipt.path, &backup)?;
    let restored = inspect_existing(&receipt.path);
    if !restored.clean {
        let _ = atomic_replace(&receipt.path, &current);
        return Err(Error::Config(
            "Devin extension restore verification failed; the patched file was put back".into(),
        ));
    }
    let _ = fs::remove_file(receipt_path(&receipt.path));
    Ok(())
}

/// Restore a verified receipt left by an unclean exit. Returns whether a host
/// file was actually changed.
pub fn restore_persisted(path: &Path) -> bool {
    let status = inspect_existing(path);
    let Some(receipt) = status.restore_receipt else {
        return false;
    };
    match restore(&receipt) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                %error,
                path = %path.display(),
                "cannot restore the Devin extension gate after an unclean exit"
            );
            false
        }
    }
}

impl GateStatus {
    fn not_found() -> Self {
        Self {
            path: None,
            version: "unknown".into(),
            compatible: false,
            clean: false,
            patched: false,
            backup_path: None,
            backup_available: false,
            current_sha256: String::new(),
            backup_sha256: None,
            restore_receipt: None,
            message: "Devin extension.js not found; start Devin once or reinstall it before \
                      enabling this integration"
                .into(),
        }
    }
}

fn apply_with(path: &Path) -> Result<GateReceipt> {
    recover_displaced(path);
    let current = fs::read(path).map_err(|error| host_read_error(path, &error))?;
    let content = std::str::from_utf8(&current)
        .map_err(|_| Error::Config("Devin extension is not UTF-8; patch refused".into()))?;
    let status = inspect_existing(path);
    if !status.clean {
        return Err(Error::Config(refusal_message(&status)));
    }

    let patched = patch_content(content)?;
    let backup_path = backup_path(path);
    ensure_backup(&backup_path, &current)?;
    let receipt = GateReceipt {
        path: path.to_owned(),
        backup_path,
        version: status.version,
        original_sha256: sha256(&current),
        patched_sha256: sha256(patched.as_bytes()),
    };
    write_receipt(&receipt)?;
    if let Err(error) = atomic_replace(path, patched.as_bytes()) {
        if !fs::read(path).is_ok_and(|bytes| bytes == patched.as_bytes()) {
            discard_failed_receipt(path, &current);
            return Err(error);
        }
    }

    let verified = inspect_existing(path);
    if verified.patched && verified.current_sha256 == receipt.patched_sha256 {
        Ok(receipt)
    } else {
        let _ = atomic_replace(path, &current);
        discard_failed_receipt(path, &current);
        Err(Error::Config(format!(
            "Devin extension patch verification failed: {}",
            verified.message
        )))
    }
}

fn inspect_existing(path: &Path) -> GateStatus {
    recover_displaced(path);
    let version = product_version(path);
    let backup_path = backup_path(path);
    let (backup_available, backup_sha256) = match fs::read(&backup_path) {
        Ok(bytes) => (true, Some(sha256(&bytes))),
        Err(_) => (false, None),
    };
    let current = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return GateStatus {
                path: Some(path.to_owned()),
                version,
                compatible: false,
                clean: false,
                patched: false,
                backup_path: Some(backup_path),
                backup_available,
                current_sha256: String::new(),
                backup_sha256,
                restore_receipt: None,
                message: host_read_error(path, &error).to_string(),
            }
        }
    };
    let current_sha256 = sha256(&current);
    let content = match std::str::from_utf8(&current) {
        Ok(content) => content,
        Err(_) => {
            return GateStatus {
                path: Some(path.to_owned()),
                version,
                compatible: false,
                clean: false,
                patched: false,
                backup_path: Some(backup_path),
                backup_available,
                current_sha256,
                backup_sha256,
                restore_receipt: None,
                message: "Devin extension is not UTF-8; patch and restore are refused".into(),
            }
        }
    };
    let clean_count = gate_occurrences(content, CLEAN_GATE_PATTERN).len();
    let patched_count = gate_occurrences(content, PATCHED_GATE_PATTERN).len();
    let clean = clean_count == 1 && patched_count == 0;
    let patched = clean_count == 0 && patched_count == 1;
    let compatible = clean || patched;
    let message = if compatible {
        String::new()
    } else {
        format!(
            "Devin {version} extension does not contain exactly one recognizable endpoint settings \
             gate (clean={clean_count}, patched={patched_count}); its code context may have \
             changed; update haxsd byok before writing"
        )
    };
    let mut status = GateStatus {
        path: Some(path.to_owned()),
        version,
        compatible,
        clean,
        patched,
        backup_path: Some(backup_path),
        backup_available,
        current_sha256,
        backup_sha256,
        restore_receipt: None,
        message,
    };
    status.restore_receipt = usable_receipt(path, &status);
    status
}

fn gate_occurrences<'content>(
    content: &'content str,
    pattern: &str,
) -> Vec<(std::ops::Range<usize>, &'content str)> {
    Regex::new(pattern)
        .expect("static endpoint gate regex")
        .captures_iter(content)
        .filter_map(|captures| {
            let matched = captures.get(0)?;
            let mut from = matched.start().saturating_sub(GATE_CONTEXT_WINDOW);
            while !content.is_char_boundary(from) {
                from -= 1;
            }
            let mut until = (matched.end() + GATE_CONTEXT_WINDOW).min(content.len());
            while !content.is_char_boundary(until) {
                until += 1;
            }
            let context = &content[from..until];
            GATE_CONTEXT_ANCHORS
                .iter()
                .all(|anchor| context.contains(anchor))
                .then(|| {
                    (
                        matched.range(),
                        captures.get(1).expect("return name").as_str(),
                    )
                })
        })
        .collect()
}

fn product_version(path: &Path) -> String {
    path.ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "app"))
        .and_then(|app| fs::read(app.join("package.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|package| package.get("version")?.as_str().map(str::to_owned))
        .filter(|version| !version.trim().is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn patch_content(content: &str) -> Result<String> {
    let gates = gate_occurrences(content, CLEAN_GATE_PATTERN);
    if gates.len() != 1 || !gate_occurrences(content, PATCHED_GATE_PATTERN).is_empty() {
        return Err(Error::Config(
            "Devin endpoint settings gate is missing or ambiguous; patch refused".into(),
        ));
    }
    let (range, return_name) = &gates[0];
    let mut patched = content.to_owned();
    patched.replace_range(range.clone(), &format!("if(!1)return {return_name}();"));
    if gate_occurrences(&patched, PATCHED_GATE_PATTERN).len() != 1
        || !gate_occurrences(&patched, CLEAN_GATE_PATTERN).is_empty()
    {
        return Err(Error::Config(
            "Devin endpoint settings gate verification failed before writing".into(),
        ));
    }
    Ok(patched)
}

fn refusal_message(status: &GateStatus) -> String {
    if status.patched {
        return format!(
            "Devin extension already has this application's settings gate; restore the clean \
             extension first; backup: {}",
            status
                .backup_path
                .as_deref()
                .map(Path::display)
                .map(|path| path.to_string())
                .unwrap_or_else(|| "unavailable".into())
        );
    }
    if !status.message.is_empty() {
        return status.message.clone();
    }
    "Devin extension is not a supported clean bundle; patch refused".into()
}

fn restore_refusal(receipt: &GateReceipt, reason: &str) -> Error {
    let backup_usable = fs::read(&receipt.backup_path)
        .is_ok_and(|bytes| is_clean_bytes(&bytes, &receipt.original_sha256));
    if backup_usable {
        Error::Config(format!(
            "{reason}; copy {} over {} and restart Devin; backup: {}",
            receipt.backup_path.display(),
            receipt.path.display(),
            receipt.backup_path.display()
        ))
    } else {
        Error::Config(format!(
            "{reason}; the verified backup is unavailable, so reinstall Devin; backup: {}",
            receipt.backup_path.display()
        ))
    }
}

fn usable_receipt(path: &Path, status: &GateStatus) -> Option<GateReceipt> {
    let bytes = fs::read(receipt_path(path)).ok()?;
    let receipt: GateReceipt = serde_json::from_slice(&bytes).ok()?;
    if receipt.path != path || Some(receipt.backup_path.as_path()) != status.backup_path.as_deref()
    {
        return None;
    }
    if !status.patched || status.current_sha256 != receipt.patched_sha256 {
        return None;
    }
    let backup = fs::read(&receipt.backup_path).ok()?;
    if sha256(&backup) != receipt.original_sha256
        || !is_clean_bytes(&backup, &receipt.original_sha256)
    {
        return None;
    }
    Some(receipt)
}

fn is_clean_bytes(bytes: &[u8], expected_sha256: &str) -> bool {
    sha256(bytes) == expected_sha256
        && std::str::from_utf8(bytes).is_ok_and(|content| {
            gate_occurrences(content, CLEAN_GATE_PATTERN).len() == 1
                && gate_occurrences(content, PATCHED_GATE_PATTERN).is_empty()
        })
}

fn write_receipt(receipt: &GateReceipt) -> Result<()> {
    let path = receipt_path(&receipt.path);
    let bytes = serde_json::to_vec(receipt).map_err(|error| {
        Error::Config(format!(
            "cannot encode the Devin extension restore receipt: {error}"
        ))
    })?;
    atomic_create(&path, &bytes).map_err(|error| {
        Error::Config(format!(
            "cannot persist the Devin extension restore receipt {} ({error}); the extension was \
             left unchanged",
            path.display()
        ))
    })
}

fn discard_failed_receipt(path: &Path, original: &[u8]) {
    if (path.is_file() && fs::read(path).is_ok_and(|bytes| bytes == original)) || !path.exists() {
        let _ = fs::remove_file(receipt_path(path));
    }
}

fn ensure_backup(path: &Path, source: &[u8]) -> Result<()> {
    if fs::read(path).is_ok_and(|existing| existing == source) {
        return Ok(());
    }
    remove_if_exists(path)?;
    atomic_create(path, source)
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

fn host_read_error(path: &Path, error: &std::io::Error) -> Error {
    let backup = backup_path(path);
    let hint = backup.is_file().then(|| {
        format!(
            "; copy the verified backup {} over {} after closing Devin; backup: {}",
            backup.display(),
            path.display(),
            backup.display()
        )
    });
    Error::Config(format!(
        "cannot read Devin extension {}: {error}; check the installation and try again{}",
        path.display(),
        hint.unwrap_or_default()
    ))
}

fn atomic_create(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config("Devin extension backup path has no parent".into()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(Error::Io)?;
    temporary.write_all(bytes).map_err(Error::Io)?;
    temporary.flush().map_err(Error::Io)?;
    temporary.as_file().sync_all().map_err(Error::Io)?;
    if temporary.as_file().metadata().map_err(Error::Io)?.len() != bytes.len() as u64 {
        return Err(Error::Config(
            "Devin extension staging size mismatch".into(),
        ));
    }
    temporary
        .persist(path)
        .map_err(|error| Error::Io(error.error))?;
    Ok(())
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config("Devin extension has no parent directory".into()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(Error::Io)?;
    temporary.write_all(bytes).map_err(Error::Io)?;
    temporary.flush().map_err(Error::Io)?;
    temporary.as_file().sync_all().map_err(Error::Io)?;
    if temporary.as_file().metadata().map_err(Error::Io)?.len() != bytes.len() as u64 {
        return Err(Error::Config(
            "Devin extension replacement staging size mismatch".into(),
        ));
    }
    if !path.exists() {
        return temporary
            .persist(path)
            .map(|_| ())
            .map_err(|error| Error::Io(error.error));
    }
    let displaced = displaced_path(path);
    let _ = fs::remove_file(&displaced);
    fs::rename(path, &displaced).map_err(Error::Io)?;
    if let Err(error) = temporary.persist(path).map_err(|error| error.error) {
        let _ = fs::rename(&displaced, path);
        return Err(Error::Io(error));
    }
    let _ = fs::remove_file(&displaced);
    Ok(())
}

fn recover_displaced(path: &Path) {
    let displaced = displaced_path(path);
    if path.exists() || !displaced.is_file() {
        return;
    }
    if let Err(error) = fs::rename(&displaced, path) {
        tracing::warn!(
            %error,
            path = %path.display(),
            "cannot put back the displaced Devin extension"
        );
    }
}

/// 同 `host_config::with_suffix`：追加后缀不能经过 `Path::display()`。
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(suffix);
    PathBuf::from(raw)
}

fn displaced_path(path: &Path) -> PathBuf {
    with_suffix(path, ".devin-router-gate-replace")
}

fn backup_path(path: &Path) -> PathBuf {
    with_suffix(path, BACKUP_SUFFIX)
}

fn receipt_path(path: &Path) -> PathBuf {
    with_suffix(path, RECEIPT_SUFFIX)
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes)).to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLEAN_GATE: &str = "if(!R()&&!(0,p.isDevelopment)())return i();";
    const PATCHED_GATE: &str = "if(!1)return i();";

    fn fixture() -> String {
        r#"var p={isDevelopment:()=>false};function R(){return !1}
function S(e){const[t,n=""]=e.split(".",2),i=()=>({value:C(e),source:"deployment_default"});if(e===m.API_SERVER_URL){const e=null;if(void 0!==e&&""!==e)return{value:e,source:"local_account"}}if(!R()&&!(0,p.isDevelopment)())return i();const r=a.workspace.getConfiguration(t).inspect(n);if(!a.workspace.isTrusted){const e=r?.globalValue??void 0;return void 0!==e?{value:e,source:"user_setting"}:i()}const s=r?.workspaceFolderValue??r?.workspaceValue??r?.globalValue;return void 0!==s?{value:s,source:"user_setting"}:i()}
"#
        .into()
    }

    fn apply_fixture(path: &Path) -> GateReceipt {
        apply(path).unwrap()
    }

    fn inspect_fixture(path: &Path) -> GateStatus {
        inspect_existing(path)
    }

    #[test]
    fn changed_build_and_minified_names_can_be_patched_and_restored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let content = format!("const build='next';{}", fixture()).replace(
            CLEAN_GATE,
            "if(!R9()&&!(0,dev.isDevelopment)())return ret();",
        );
        fs::write(&path, &content).unwrap();

        assert!(inspect(Some(&path)).clean);
        let receipt = apply(&path).unwrap();
        assert!(inspect(Some(&path)).patched);
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("if(!1)return ret();"));
        restore(&receipt).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn identical_gate_outside_endpoint_settings_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let content = format!("function unrelated(){{{CLEAN_GATE}}}");
        fs::write(&path, &content).unwrap();

        assert!(patch_content(&content).is_err());
        assert!(!inspect(Some(&path)).compatible);
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
        assert!(!backup_path(&path).exists());
        assert!(!receipt_path(&path).exists());
    }

    #[test]
    fn patch_replaces_the_single_development_gate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let original = fixture();
        fs::write(&path, &original).unwrap();

        let receipt = apply_fixture(&path);
        let patched = fs::read_to_string(&path).unwrap();
        assert_eq!(patched.matches(PATCHED_GATE).count(), 1);
        assert_eq!(patched.matches(CLEAN_GATE).count(), 0);
        assert!(inspect_fixture(&path).patched);
        assert!(inspect_fixture(&path).restore_receipt.is_some());
        assert!(receipt.backup_path.is_file());

        restore(&receipt).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(inspect_fixture(&path).clean);
        assert!(!receipt_path(&path).exists());
    }

    #[test]
    fn ambiguous_or_missing_gate_is_refused_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let content = "function S(e){return e}";
        fs::write(&path, content).unwrap();

        let error = patch_content(content).unwrap_err().to_string();
        assert!(error.contains("missing or ambiguous"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn duplicated_endpoint_settings_gate_is_refused_before_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let content = fixture().repeat(2);
        fs::write(&path, &content).unwrap();

        let error = apply(&path).unwrap_err().to_string();
        assert!(error.contains("exactly one"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
        assert!(!backup_path(&path).exists());
        assert!(!receipt_path(&path).exists());
    }

    #[test]
    fn changed_extension_or_backup_invalidates_the_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        let receipt = apply_fixture(&path);

        fs::write(&path, "changed by an update").unwrap();
        assert!(!inspect_fixture(&path).patched);
        assert!(restore(&receipt).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "changed by an update");

        fs::write(&path, fixture()).unwrap();
        let receipt = apply_fixture(&path);
        fs::write(&receipt.backup_path, "tampered").unwrap();
        assert!(inspect_fixture(&path).restore_receipt.is_none());
        assert!(restore(&receipt).is_err());
        assert!(inspect_fixture(&path).patched);
    }

    #[test]
    fn stale_backup_is_rotated_for_the_current_extension() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let content = fixture();
        fs::write(&path, &content).unwrap();
        fs::write(backup_path(&path), "stale").unwrap();

        let receipt = apply_fixture(&path);
        assert_eq!(fs::read_to_string(&receipt.backup_path).unwrap(), content);
        restore(&receipt).unwrap();
    }

    #[test]
    fn a_copied_receipt_is_not_usable_for_another_extension() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.js");
        let second = directory.path().join("second.js");
        fs::write(&first, fixture()).unwrap();
        fs::write(&second, fixture()).unwrap();
        apply_fixture(&first);
        fs::copy(receipt_path(&first), receipt_path(&second)).unwrap();

        assert!(inspect_fixture(&second).restore_receipt.is_none());
    }

    #[test]
    fn missing_extension_reports_a_usable_next_step() {
        let status = inspect(Some(Path::new("Z:\\missing\\extension.js")));
        assert!(!status.compatible);
        assert!(status.message.contains("cannot read"), "{}", status.message);
    }

    #[test]
    fn missing_installation_is_a_status_not_a_crash() {
        let status = inspect(None);
        assert!(status.path.is_none());
        assert!(!status.compatible);
        assert!(status.message.contains("not found"), "{}", status.message);
    }

    /// 针对当前安装的 7 MB `extension.js` 做一次真实往返。路径只从测试环境读取，
    /// 普通测试永远不会碰用户安装；CI 也用 `ignored` 明确跳过。
    #[test]
    #[ignore = "requires HAXSD_BYOK_DEVIN_TEST_EXTENSION pointing at a disposable copy"]
    fn current_extension_copy_can_be_patched_and_restored() {
        let path = std::env::var_os("HAXSD_BYOK_DEVIN_TEST_EXTENSION")
            .map(PathBuf::from)
            .expect("HAXSD_BYOK_DEVIN_TEST_EXTENSION must point at a disposable extension.js copy");
        let original = fs::read(&path).unwrap();
        assert!(
            inspect(Some(&path)).clean,
            "{}",
            inspect(Some(&path)).message
        );

        let receipt = apply(&path).unwrap();
        let patched = inspect(Some(&path));
        assert!(patched.patched, "{}", patched.message);
        assert!(patched.restore_receipt.is_some());
        assert_ne!(fs::read(&path).unwrap(), original);

        restore(&receipt).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(inspect(Some(&path)).clean);
    }
}
