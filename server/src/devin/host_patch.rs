//! Structure-based access to Devin's own endpoint settings.
//!
//! Devin only reads `codeium.apiServerUrl` and
//! `codeium.inferenceApiServerUrl` when the `Codeium.codeium-dev` extension is
//! present. Packaged builds therefore ignore those user settings and keep using
//! the official endpoints. This module removes that one development-only gate
//! from a uniquely identified endpoint settings function. When a signed-in API
//! address is present, it remains the fallback after explicit settings rather
//! than taking precedence over them. A signed-in session must also not restart
//! the language server onto its own API address when an explicit setting exists.
//! Workspace trust rules stay unchanged.
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
const JS_IDENTIFIER: &str = r"[A-Za-z_$][A-Za-z0-9_$]*";
const SESSION_BRANCH_PATTERN: &str = concat!(
    r#"if\((?P<session_key>[A-Za-z_$][A-Za-z0-9_$]*)===[A-Za-z_$][A-Za-z0-9_$]*\.API_SERVER_URL\)\{"#,
    r#"const (?P<value>[A-Za-z_$][A-Za-z0-9_$]*)=\(0,[A-Za-z_$][A-Za-z0-9_$]*\.getSessionApiServerUrl\)\(\);"#,
    r#"if\(void 0!==(?P<defined>[A-Za-z_$][A-Za-z0-9_$]*)&&""!==(?P<nonempty>[A-Za-z_$][A-Za-z0-9_$]*)\)return\{value:(?P<returned>[A-Za-z_$][A-Za-z0-9_$]*),source:"auth_session"\}\}"#,
);
const DEPLOYMENT_VALUE_PATTERN: &str = r#"\{value:[A-Za-z_$][A-Za-z0-9_$]*\((?P<key>[A-Za-z_$][A-Za-z0-9_$]*)\),source:"deployment_default"\}"#;
const LOCAL_ACCOUNT_BRANCH_PATTERN: &str = concat!(
    r#"if\((?P<session_key>[A-Za-z_$][A-Za-z0-9_$]*)===[A-Za-z_$][A-Za-z0-9_$]*\.API_SERVER_URL\)\{"#,
    r#"const (?P<value>[A-Za-z_$][A-Za-z0-9_$]*)=\(0,[A-Za-z_$][A-Za-z0-9_$]*\.getLocalAccount\)\(\)\?\.apiServerUrl;"#,
    r#"if\(void 0!==(?P<defined>[A-Za-z_$][A-Za-z0-9_$]*)&&""!==(?P<nonempty>[A-Za-z_$][A-Za-z0-9_$]*)\)return\{value:(?P<returned>[A-Za-z_$][A-Za-z0-9_$]*),source:"local_account"\}\}"#,
);
const LS_RESTART_WINDOW: usize = 800;
const LS_METHOD_RESTART_WINDOW: usize = 4_096;
const LS_VENDOR_RESTART_PATTERN: &str = concat!(
    r#"await async function\(e\)\{let t;try\{t=(?P<client>[A-Za-z_$][A-Za-z0-9_$]*)\.LanguageServerClient\.getInstance\(\)\}catch\{return\}"#,
    r#"e!==t\.apiServerUrl&&await t\.restart\(e\)\}\(t\)"#,
);
const LS_PATCHED_RESTART_PATTERN: &str = concat!(
    r#"await async function\(e\)\{let t;try\{t=(?P<client>[A-Za-z_$][A-Za-z0-9_$]*)\.LanguageServerClient\.getInstance\(\)\}catch\{return\}"#,
    r#"const n=\(0,(?P<diagnostics>[A-Za-z_$][A-Za-z0-9_$]*)\.getApiServerUrlDiagnostics\)\(\),i="user_setting"===n\.source\?\(0,(?P<config>[A-Za-z_$][A-Za-z0-9_$]*)\.getConfig\)\((?P<config_again>[A-Za-z_$][A-Za-z0-9_$]*)\.Config\.API_SERVER_URL\):e;"#,
    r#"i!==t\.apiServerUrl&&await t\.restart\(i\)\}\(t\)"#,
);
const LS_METHOD_VENDOR_RESTART_PATTERN: &str = concat!(
    r#"async restartLanguageServerIfNeeded\((?P<arg>[A-Za-z_$][A-Za-z0-9_$]*)\)\{\("#,
    r#"0,(?P<isstring>[A-Za-z_$][A-Za-z0-9_$]*)\.isString\)\((?P<arg2>[A-Za-z_$][A-Za-z0-9_$]*)\)&&!\("#,
    r#"0,(?P<isempty>[A-Za-z_$][A-Za-z0-9_$]*)\.isEmpty\)\((?P<arg3>[A-Za-z_$][A-Za-z0-9_$]*)\)&&(?P<arg4>[A-Za-z_$][A-Za-z0-9_$]*)!==(?P<client>[A-Za-z_$][A-Za-z0-9_$]*)\.LanguageServerClient\.getInstance\(\)\.apiServerUrl&&await\s*(?P<client2>[A-Za-z_$][A-Za-z0-9_$]*)\.LanguageServerClient\.getInstance\(\)\.restart\((?P<arg5>[A-Za-z_$][A-Za-z0-9_$]*)\)\}"#,
);
const LS_METHOD_PATCHED_RESTART_PATTERN: &str = r#"i!==(?P<client>[A-Za-z_$][A-Za-z0-9_$]*)\.LanguageServerClient\.getInstance\(\)\.apiServerUrl&&await\s*(?P<client2>[A-Za-z_$][A-Za-z0-9_$]*)\.LanguageServerClient\.getInstance\(\)\.restart\(i\)"#;
const LS_VENDOR_RESTART_TERNARY_PATTERN: &str = concat!(
    r#"await async function\(e\)\{let t;try\{t=(?P<client>[A-Za-z_$][A-Za-z0-9_$]*)\.LanguageServerClient\.getInstance\(\)\}catch\{return\}"#,
    r#"e!==t\.apiServerUrl\?await t\.restart\(e\):await t\.whenProcessReplaced\(\)\}\(t\)"#,
);
const LS_PATCHED_RESTART_TERNARY_PATTERN: &str = concat!(
    r#"await async function\(e\)\{let t;try\{t=(?P<client>[A-Za-z_$][A-Za-z0-9_$]*)\.LanguageServerClient\.getInstance\(\)\}catch\{return\}"#,
    r#"const n=\(0,(?P<diagnostics>[A-Za-z_$][A-Za-z0-9_$]*)\.getApiServerUrlDiagnostics\)\(\),i="user_setting"===n\.source\?\(0,(?P<config>[A-Za-z_$][A-Za-z0-9_$]*)\.getConfig\)\((?P<config_again>[A-Za-z_$][A-Za-z0-9_$]*)\.Config\.API_SERVER_URL\):e;"#,
    r#"i!==t\.apiServerUrl\?await t\.restart\(i\):await t\.whenProcessReplaced\(\)\}\(t\)"#,
);
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
    let (base, original, repair) = if status.clean && status.compatible {
        (content.to_owned(), current.clone(), false)
    } else if status.patched {
        let receipt = status
            .restore_receipt
            .clone()
            .ok_or_else(|| Error::Config(refusal_message(&status)))?;
        let backup = fs::read(&receipt.backup_path).map_err(|error| {
            Error::Config(format!(
                "Devin extension backup {} is unavailable ({error}); restore refused; backup: {}",
                receipt.backup_path.display(),
                receipt.backup_path.display()
            ))
        })?;
        if sha256(&backup) != receipt.original_sha256
            || !is_clean_bytes(&backup, &receipt.original_sha256)
        {
            return Err(Error::Config(format!(
                "Devin extension backup {} does not match the verified clean file; restore \
                 refused; backup: {}",
                receipt.backup_path.display(),
                receipt.backup_path.display()
            )));
        }
        let base = std::str::from_utf8(&backup).map_err(|_| {
            Error::Config("Devin extension backup is not UTF-8; patch refused".into())
        })?;
        (base.to_owned(), backup, true)
    } else {
        return Err(Error::Config(refusal_message(&status)));
    };

    let patched = patch_content(&base)?;
    let backup_path = backup_path(path);
    if repair {
        // A gate-only or partially applied patch is repaired from its verified
        // clean backup. Put the clean file back before writing the new patch
        // so a failure leaves a usable client, never another half-patch.
        atomic_replace(path, &original)?;
        let _ = fs::remove_file(receipt_path(path));
    }
    ensure_backup(&backup_path, &original)?;
    let receipt = GateReceipt {
        path: path.to_owned(),
        backup_path,
        version: status.version,
        original_sha256: sha256(&original),
        patched_sha256: sha256(patched.as_bytes()),
    };
    write_receipt(&receipt)?;
    if let Err(error) = atomic_replace(path, patched.as_bytes()) {
        if !fs::read(path).is_ok_and(|bytes| bytes == patched.as_bytes()) {
            discard_failed_receipt(path, &original);
            return Err(error);
        }
    }

    let verified = inspect_existing(path);
    if verified.patched && verified.compatible && verified.current_sha256 == receipt.patched_sha256
    {
        Ok(receipt)
    } else {
        let _ = atomic_replace(path, &original);
        discard_failed_receipt(path, &original);
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
    let clean_gates = gate_occurrences(content, CLEAN_GATE_PATTERN);
    let patched_gates = gate_occurrences(content, PATCHED_GATE_PATTERN);
    let clean_count = clean_gates.len();
    let patched_count = patched_gates.len();
    let clean = clean_count == 1 && patched_count == 0;
    // `patched` also preserves restoration of a verified earlier patch. Only
    // `compatible && patched` means that this build honors endpoint settings.
    let patched = clean_count == 0 && patched_count == 1;
    let pre_setting_update = if clean || patched {
        let (range, fallback) = clean_gates.first().or(patched_gates.first()).unwrap();
        pre_setting_fallback_update(content, range.start, fallback)
    } else {
        Ok(None)
    };
    let restart_update = language_server_restart_update(content);
    let needs_refresh = patched
        && (matches!(&pre_setting_update, Ok(Some(_))) || matches!(&restart_update, Ok(Some(_))));
    let compatible = (clean || patched)
        && pre_setting_update.is_ok()
        && restart_update.is_ok()
        && !needs_refresh;
    let message = if needs_refresh {
        format!(
            "Devin {version} endpoint settings patch needs refresh: a local-account or signed-in \
             API address still overrides an explicit setting or the language server; restore the \
             existing integration before applying it again"
        )
    } else if let Err(error) = &pre_setting_update {
        error.to_string()
    } else if let Err(error) = &restart_update {
        error.to_string()
    } else if compatible {
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

/// Keep a signed-in/local-account endpoint as the fallback after the vendor's
/// trusted/untrusted setting selection. This recognizes both the old session
/// branch and 3.12.59's local-account branch; unfamiliar code is never rewritten.
fn fallback_update(
    content: &str,
    gate_start: usize,
    fallback: &str,
    branch_pattern: &str,
    getter: &str,
    source: &str,
    allow_dead_branch: bool,
) -> Result<Option<(std::ops::Range<usize>, String)>> {
    let mut from = gate_start.saturating_sub(GATE_CONTEXT_WINDOW);
    while !content.is_char_boundary(from) {
        from -= 1;
    }
    let prefix = &content[from..gate_start];
    let has_getter = prefix.contains(getter);
    let has_source = prefix.contains(&format!("source:\"{source}\""));
    if !has_getter {
        if !has_source {
            return Ok(None);
        }
        if allow_dead_branch && prefix.contains("const e=null;") {
            return Ok(None);
        }
        return Err(Error::Config(format!(
            "Devin endpoint settings {source} fallback is unrecognized or inconsistent; patch refused"
        )));
    }

    let before = Regex::new(&format!(
        r"(?P<fallback>{JS_IDENTIFIER})=\(\)=>\((?P<default>{DEPLOYMENT_VALUE_PATTERN})\);(?P<session>{branch_pattern})$"
    ))
    .expect("static endpoint prefix regex");
    let after = Regex::new(&format!(
        r"(?P<fallback>{JS_IDENTIFIER})=\(\)=>\{{(?P<session>{branch_pattern})return(?P<default>{DEPLOYMENT_VALUE_PATTERN})\}};$"
    ))
    .expect("static endpoint fallback regex");
    let refusal = || {
        Error::Config(format!(
            "Devin endpoint settings {source} fallback is unrecognized or inconsistent; patch refused"
        ))
    };
    let (captures, reorder) = if let Some(captures) = before.captures(prefix) {
        (captures, true)
    } else if let Some(captures) = after.captures(prefix) {
        (captures, false)
    } else {
        return Err(refusal());
    };
    if &captures["fallback"] != fallback
        || captures["key"] != captures["session_key"]
        || ["defined", "nonempty", "returned"]
            .iter()
            .any(|name| captures[*name] != captures["value"])
    {
        return Err(refusal());
    }
    if !reorder {
        return Ok(None);
    }
    let matched = captures.get(0).unwrap();
    Ok(Some((
        from + matched.start()..from + matched.end(),
        format!(
            "{fallback}=()=>{{{}return{}}};",
            &captures["session"], &captures["default"]
        ),
    )))
}

fn pre_setting_fallback_update(
    content: &str,
    gate_start: usize,
    fallback: &str,
) -> Result<Option<(std::ops::Range<usize>, String)>> {
    let mut from = gate_start.saturating_sub(GATE_CONTEXT_WINDOW);
    while !content.is_char_boundary(from) {
        from -= 1;
    }
    let prefix = &content[from..gate_start];
    let has_session =
        prefix.contains("getSessionApiServerUrl") || prefix.contains("source:\"auth_session\"");
    let has_local =
        prefix.contains("getLocalAccount") || prefix.contains("source:\"local_account\"");
    if has_session && has_local {
        return Err(Error::Config(
            "Devin endpoint settings has both session and local-account overrides; patch refused"
                .into(),
        ));
    }
    if has_session {
        fallback_update(
            content,
            gate_start,
            fallback,
            SESSION_BRANCH_PATTERN,
            "getSessionApiServerUrl",
            "auth_session",
            false,
        )
    } else if has_local {
        fallback_update(
            content,
            gate_start,
            fallback,
            LOCAL_ACCOUNT_BRANCH_PATTERN,
            "getLocalAccount",
            "local_account",
            true,
        )
    } else {
        Ok(None)
    }
}

fn api_server_config_binding(content: &str, match_start: usize, window: usize) -> Result<String> {
    let mut from = match_start.saturating_sub(window);
    while !content.is_char_boundary(from) {
        from -= 1;
    }
    let config = Regex::new(&format!(
        r"\(0,(?P<config>{JS_IDENTIFIER})\.getConfig\)\((?P<config_again>{JS_IDENTIFIER})\.Config\.API_SERVER_URL\)"
    ))
    .expect("static API config binding regex");
    config
        .captures_iter(&content[from..match_start])
        .filter(|captures| captures["config"] == captures["config_again"])
        .last()
        .map(|captures| captures["config"].to_string())
        .ok_or_else(|| {
            Error::Config(
                "Devin language-server API restart has no recognizable settings binding; patch refused"
                    .into(),
            )
        })
}

/// A signed-in session or local account restarts the language server with its
/// own API address. An explicit user setting must win; without one, the
/// signed-in/local-account address stays.
fn language_server_restart_update(
    content: &str,
) -> Result<Option<(std::ops::Range<usize>, String)>> {
    static VENDOR_OLD: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static PATCHED_OLD: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static VENDOR_NEW: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static PATCHED_NEW: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static VENDOR_TERNARY: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static PATCHED_TERNARY: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();

    let vendor_old = VENDOR_OLD.get_or_init(|| {
        Regex::new(LS_VENDOR_RESTART_PATTERN).expect("static language-server restart regex")
    });
    let patched_old = PATCHED_OLD.get_or_init(|| {
        Regex::new(LS_PATCHED_RESTART_PATTERN)
            .expect("static patched language-server restart regex")
    });
    let vendor_new = VENDOR_NEW.get_or_init(|| {
        Regex::new(LS_METHOD_VENDOR_RESTART_PATTERN)
            .expect("static language-server method restart regex")
    });
    let patched_new = PATCHED_NEW.get_or_init(|| {
        Regex::new(LS_METHOD_PATCHED_RESTART_PATTERN)
            .expect("static patched language-server method restart regex")
    });
    let vendor_ternary = VENDOR_TERNARY.get_or_init(|| {
        Regex::new(LS_VENDOR_RESTART_TERNARY_PATTERN)
            .expect("static language-server ternary restart regex")
    });
    let patched_ternary = PATCHED_TERNARY.get_or_init(|| {
        Regex::new(LS_PATCHED_RESTART_TERNARY_PATTERN)
            .expect("static patched language-server ternary restart regex")
    });

    let vendor_old_matches = vendor_old.captures_iter(content).collect::<Vec<_>>();
    let patched_old_matches = patched_old
        .captures_iter(content)
        .filter(|captures| {
            captures["diagnostics"] == captures["config"]
                && captures["config"] == captures["config_again"]
        })
        .collect::<Vec<_>>();
    let vendor_new_matches = vendor_new
        .captures_iter(content)
        .filter(|captures| {
            ["arg", "arg2", "arg3", "arg4", "arg5"]
                .iter()
                .all(|name| captures[*name] == captures["arg"])
                && captures["client"] == captures["client2"]
        })
        .collect::<Vec<_>>();
    let patched_new_matches = patched_new
        .captures_iter(content)
        .filter(|captures| captures["client"] == captures["client2"])
        .collect::<Vec<_>>();
    let vendor_ternary_matches = vendor_ternary.captures_iter(content).collect::<Vec<_>>();
    let patched_ternary_matches = patched_ternary
        .captures_iter(content)
        .filter(|captures| {
            captures["diagnostics"] == captures["config"]
                && captures["config"] == captures["config_again"]
        })
        .collect::<Vec<_>>();

    let matches = vendor_old_matches.len()
        + patched_old_matches.len()
        + vendor_new_matches.len()
        + patched_new_matches.len()
        + vendor_ternary_matches.len()
        + patched_ternary_matches.len();
    if matches > 1 {
        return Err(Error::Config(
            "Devin language-server API restart is ambiguous; patch refused".into(),
        ));
    }

    if let Some(captures) = vendor_ternary_matches.first() {
        let matched = captures
            .get(0)
            .expect("language-server ternary restart match");
        let config = api_server_config_binding(content, matched.start(), LS_RESTART_WINDOW)?;
        let client = &captures["client"];
        return Ok(Some((
            matched.range(),
            format!(
                "await async function(e){{let t;try{{t={client}.LanguageServerClient.getInstance()}}catch{{return}}const n=(0,{config}.getApiServerUrlDiagnostics)(),i=\"user_setting\"===n.source?(0,{config}.getConfig)({config}.Config.API_SERVER_URL):e;i!==t.apiServerUrl?await t.restart(i):await t.whenProcessReplaced()}}(t)"
            ),
        )));
    }

    if let Some(captures) = vendor_old_matches.first() {
        let matched = captures.get(0).expect("language-server restart match");
        let mut from = matched.start().saturating_sub(LS_RESTART_WINDOW);
        while !content.is_char_boundary(from) {
            from -= 1;
        }
        let config = Regex::new(&format!(
            r"\(0,(?P<config>{JS_IDENTIFIER})\.getConfig\)\((?P<config_again>{JS_IDENTIFIER})\.Config\.API_SERVER_URL\)"
        ))
        .expect("static API config binding regex");
        let Some(config) = config
            .captures_iter(&content[from..matched.start()])
            .filter(|captures| captures["config"] == captures["config_again"])
            .last()
        else {
            return Err(Error::Config(
                "Devin language-server API restart has no recognizable settings binding; patch refused"
                    .into(),
            ));
        };
        let client = &captures["client"];
        let config = &config["config"];
        return Ok(Some((
            matched.range(),
            format!(
                "await async function(e){{let t;try{{t={client}.LanguageServerClient.getInstance()}}catch{{return}}const n=(0,{config}.getApiServerUrlDiagnostics)(),i=\"user_setting\"===n.source?(0,{config}.getConfig)({config}.Config.API_SERVER_URL):e;i!==t.apiServerUrl&&await t.restart(i)}}(t)"
            ),
        )));
    }

    if let Some(captures) = vendor_new_matches.first() {
        let matched = captures
            .get(0)
            .expect("language-server method restart match");
        let mut from = matched.start().saturating_sub(LS_METHOD_RESTART_WINDOW);
        while !content.is_char_boundary(from) {
            from -= 1;
        }
        let config = Regex::new(&format!(
            r"\(0,(?P<config>{JS_IDENTIFIER})\.getConfig\)\((?P<config_again>{JS_IDENTIFIER})\.Config\.API_SERVER_URL\)"
        ))
        .expect("static API config binding regex");
        let Some(config) = config
            .captures_iter(&content[from..matched.start()])
            .filter(|captures| captures["config"] == captures["config_again"])
            .last()
        else {
            return Err(Error::Config(
                "Devin language-server method restart has no recognizable settings binding; patch refused"
                    .into(),
            ));
        };
        let arg = &captures["arg"];
        let isstring = &captures["isstring"];
        let isempty = &captures["isempty"];
        let client = &captures["client"];
        let config = &config["config"];
        return Ok(Some((
            matched.range(),
            format!(
                "async restartLanguageServerIfNeeded({arg}){{const n=(0,{config}.getApiServerUrlDiagnostics)(),i=\"user_setting\"===n.source?(0,{config}.getConfig)({config}.Config.API_SERVER_URL):{arg};(0,{isstring}.isString)(i)&&!(0,{isempty}.isEmpty)(i)&&i!=={client}.LanguageServerClient.getInstance().apiServerUrl&&await {client}.LanguageServerClient.getInstance().restart(i)}}"
            ),
        )));
    }

    let has_restart =
        content.contains("LanguageServerClient.getInstance") && content.contains(".restart(");
    if has_restart
        && vendor_old_matches.is_empty()
        && patched_old_matches.is_empty()
        && vendor_new_matches.is_empty()
        && patched_new_matches.is_empty()
        && vendor_ternary_matches.is_empty()
        && patched_ternary_matches.is_empty()
    {
        return Err(Error::Config(
            "Devin language-server restart method is unrecognized; patch refused".into(),
        ));
    }
    Ok(None)
}

fn patch_content(content: &str) -> Result<String> {
    let gates = gate_occurrences(content, CLEAN_GATE_PATTERN);
    if gates.len() != 1 || !gate_occurrences(content, PATCHED_GATE_PATTERN).is_empty() {
        return Err(Error::Config(
            "Devin endpoint settings gate is missing or ambiguous; patch refused".into(),
        ));
    }
    let (range, return_name) = &gates[0];
    let pre_setting_update = pre_setting_fallback_update(content, range.start, return_name)?;
    let restart_update = language_server_restart_update(content)?;
    let mut edits = vec![(range.clone(), format!("if(!1)return {return_name}();"))];
    if let Some(update) = pre_setting_update {
        edits.push(update);
    }
    if let Some(update) = restart_update {
        edits.push(update);
    }
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut patched = content.to_owned();
    for (range, replacement) in edits {
        patched.replace_range(range, &replacement);
    }
    let patched_gates = gate_occurrences(&patched, PATCHED_GATE_PATTERN);
    if patched_gates.len() != 1
        || !gate_occurrences(&patched, CLEAN_GATE_PATTERN).is_empty()
        || pre_setting_fallback_update(&patched, patched_gates[0].0.start, return_name)?.is_some()
        || language_server_restart_update(&patched)?.is_some()
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

    fn logged_in_fixture() -> String {
        fixture()
            .replace("const e=null;", "const e=(0,l.getSessionApiServerUrl)();")
            .replace("source:\"local_account\"", "source:\"auth_session\"")
    }

    fn local_account_fixture() -> String {
        fixture().replace(
            "const e=null;",
            "const e=(0,l.getLocalAccount)()?.apiServerUrl;",
        )
    }

    fn local_account_restart_fixture() -> String {
        format!(
            "{}\nclass Auth{{static getSessionsSecretKey(){{return(0,T.isStaging)((0,_.getConfig)(_.Config.API_SERVER_URL))?\"a\":\"b\"}}async restartLanguageServerIfNeeded(e){{(0,u.isString)(e)&&!(0,u.isEmpty)(e)&&e!==A.LanguageServerClient.getInstance().apiServerUrl&&await A.LanguageServerClient.getInstance().restart(e)}}}}",
            local_account_fixture()
        )
    }

    const VENDOR_TERNARY_RESTART: &str = "await async function(e){let t;try{t=A.LanguageServerClient.getInstance()}catch{return}e!==t.apiServerUrl?await t.restart(e):await t.whenProcessReplaced()}(t)";
    const PATCHED_TERNARY_RESTART: &str = "await async function(e){let t;try{t=A.LanguageServerClient.getInstance()}catch{return}const n=(0,_.getApiServerUrlDiagnostics)(),i=\"user_setting\"===n.source?(0,_.getConfig)(_.Config.API_SERVER_URL):e;i!==t.apiServerUrl?await t.restart(i):await t.whenProcessReplaced()}(t)";

    fn ternary_restart_fixture() -> String {
        format!(
            "{}\nconst api=(0,_.getConfig)(_.Config.API_SERVER_URL);class Auth{{async apply(){{{VENDOR_TERNARY_RESTART}}}}}",
            logged_in_fixture()
        )
    }

    #[test]
    fn logged_in_api_is_only_a_fallback_after_explicit_settings() {
        let original = logged_in_fixture();
        let patched = patch_content(&original).unwrap();
        let expected = r#"i=()=>{if(e===m.API_SERVER_URL){const e=(0,l.getSessionApiServerUrl)();if(void 0!==e&&""!==e)return{value:e,source:"auth_session"}}return{value:C(e),source:"deployment_default"}};if(!1)return i();const r=a.workspace.getConfiguration(t).inspect(n);"#;
        assert!(patched.contains(expected), "{patched}");
        assert!(patched.ends_with(
            original
                .split("const r=")
                .nth(1)
                .map(|tail| format!("const r={tail}"))
                .unwrap()
                .as_str()
        ));
        assert_eq!(patched.matches("getSessionApiServerUrl").count(), 1);
    }

    #[test]
    fn ternary_language_server_restart_is_patched_and_compatible() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let original = ternary_restart_fixture();
        fs::write(&path, &original).unwrap();

        let receipt = apply(&path).unwrap();
        let patched = fs::read_to_string(&path).unwrap();
        assert!(patched.contains(PATCHED_TERNARY_RESTART), "{patched}");
        assert!(!patched.contains(VENDOR_TERNARY_RESTART), "{patched}");
        let status = inspect(Some(&path));
        assert!(status.patched && status.compatible, "{}", status.message);

        restore(&receipt).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn a_gate_only_ternary_patch_is_repaired_from_its_clean_backup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let original = ternary_restart_fixture();
        let full = patch_content(&original).unwrap();
        let previous_patch = full.replace(PATCHED_TERNARY_RESTART, VENDOR_TERNARY_RESTART);
        assert_ne!(previous_patch, full);
        fs::write(&path, &previous_patch).unwrap();
        fs::write(backup_path(&path), &original).unwrap();
        let previous_receipt = GateReceipt {
            path: path.clone(),
            backup_path: backup_path(&path),
            version: "unknown".into(),
            original_sha256: sha256(original.as_bytes()),
            patched_sha256: sha256(previous_patch.as_bytes()),
        };
        write_receipt(&previous_receipt).unwrap();

        let status = inspect(Some(&path));
        assert!(status.patched);
        assert!(!status.compatible);
        assert!(status.message.contains("refresh"), "{}", status.message);
        assert_eq!(status.restore_receipt, Some(previous_receipt));

        let receipt = apply(&path).unwrap();
        let status = inspect(Some(&path));
        assert!(status.patched && status.compatible, "{}", status.message);
        assert_eq!(fs::read_to_string(&path).unwrap(), full);

        restore(&receipt).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn an_unrecognized_language_server_restart_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let original =
            ternary_restart_fixture().replace("whenProcessReplaced", "whenProcessReplacedX");
        fs::write(&path, &original).unwrap();

        assert!(!inspect(Some(&path)).compatible);
        assert!(apply(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn a_gate_only_patch_requires_refresh_but_keeps_its_restore_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let original = logged_in_fixture();
        let previous_patch = original.replace(CLEAN_GATE, PATCHED_GATE);
        fs::write(&path, &previous_patch).unwrap();
        fs::write(backup_path(&path), &original).unwrap();
        let receipt = GateReceipt {
            path: path.clone(),
            backup_path: backup_path(&path),
            version: "unknown".into(),
            original_sha256: sha256(original.as_bytes()),
            patched_sha256: sha256(previous_patch.as_bytes()),
        };
        write_receipt(&receipt).unwrap();

        let status = inspect(Some(&path));
        assert!(status.patched);
        assert!(
            !status.compatible,
            "a disabled gate does not establish endpoint priority"
        );
        assert!(status.message.contains("restore"), "{}", status.message);
        assert_eq!(status.restore_receipt, Some(receipt.clone()));

        restore(&receipt).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let refreshed = apply(&path).unwrap();
        let status = inspect(Some(&path));
        assert!(status.patched && status.compatible);
        restore(&refreshed).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn an_unrecognized_session_override_is_refused_before_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let original =
            logged_in_fixture().replace("getSessionApiServerUrl", "getAnotherApiServerUrl");
        fs::write(&path, &original).unwrap();

        assert!(!inspect(Some(&path)).compatible);
        assert!(apply(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!backup_path(&path).exists());
        assert!(!receipt_path(&path).exists());
    }

    #[test]
    fn local_account_and_new_language_server_restart_are_patched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let original = local_account_restart_fixture();
        fs::write(&path, &original).unwrap();

        let receipt = apply(&path).unwrap();
        let patched = fs::read_to_string(&path).unwrap();
        assert!(
            patched.contains(
                "i=()=>{if(e===m.API_SERVER_URL){const e=(0,l.getLocalAccount)()?.apiServerUrl;if(void 0!==e&&\"\"!==e)return{value:e,source:\"local_account\"}}return{value:C(e),source:\"deployment_default\"}};"
            ),
            "{patched}"
        );
        assert!(
            patched.contains("_.getApiServerUrlDiagnostics"),
            "{patched}"
        );
        assert!(
            !patched.contains(
                "e!==A.LanguageServerClient.getInstance().apiServerUrl&&await A.LanguageServerClient.getInstance().restart(e)"
            ),
            "{patched}"
        );
        let status = inspect(Some(&path));
        assert!(status.patched && status.compatible, "{}", status.message);
        assert!(status.restore_receipt.is_some());

        restore(&receipt).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(inspect(Some(&path)).clean);
    }

    #[test]
    fn gate_only_local_account_build_needs_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let original = local_account_restart_fixture();
        let gate_only = original.replace(CLEAN_GATE, PATCHED_GATE);
        fs::write(&path, &gate_only).unwrap();

        let status = inspect(Some(&path));
        assert!(status.patched);
        assert!(!status.compatible, "{}", status.message);
        assert!(status.message.contains("restore"), "{}", status.message);
        assert!(apply(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), gate_only);
    }

    #[test]
    fn unrecognized_local_account_or_new_restart_is_refused() {
        let directory = tempfile::tempdir().unwrap();

        let local = directory.path().join("local.js");
        let unknown_local = local_account_fixture().replace("getLocalAccount", "getOtherAccount");
        fs::write(&local, &unknown_local).unwrap();
        assert!(!inspect(Some(&local)).compatible);
        assert!(apply(&local).is_err());
        assert_eq!(fs::read_to_string(&local).unwrap(), unknown_local);
        assert!(!backup_path(&local).exists());
        assert!(!receipt_path(&local).exists());

        let restart = directory.path().join("restart.js");
        let unknown_restart = format!(
            "{}\nclass Auth{{async restartLanguageServerIfNeeded(e){{e&&await A.LanguageServerClient.getInstance().restart(e)}}}}",
            local_account_fixture()
        );
        fs::write(&restart, &unknown_restart).unwrap();
        assert!(!inspect(Some(&restart)).compatible);
        assert!(apply(&restart).is_err());
        assert_eq!(fs::read_to_string(&restart).unwrap(), unknown_restart);
    }

    #[test]
    fn signed_in_language_server_keeps_an_explicit_api_setting() {
        let restart = "await async function(e){let t;try{t=u.LanguageServerClient.getInstance()}catch{return}e!==t.apiServerUrl&&await t.restart(e)}(t)";
        let original = format!(
            "{}\nfunction applyAuthSession(e){{const t=e?.apiServerUrl;if((0,m.isStaging)((0,d.getConfig)(d.Config.API_SERVER_URL))){{}}{restart}}}",
            logged_in_fixture()
        );
        let patched = patch_content(&original).unwrap();
        assert!(
            patched.contains(
                "const n=(0,d.getApiServerUrlDiagnostics)(),i=\"user_setting\"===n.source?(0,d.getConfig)(d.Config.API_SERVER_URL):e;i!==t.apiServerUrl&&await t.restart(i)"
            ),
            "{patched}"
        );
        assert_eq!(patched.matches("t.restart(e)").count(), 0);
    }

    #[test]
    fn an_endpoint_priority_patch_that_still_restarts_the_language_server_needs_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let restart = "await async function(e){let t;try{t=u.LanguageServerClient.getInstance()}catch{return}e!==t.apiServerUrl&&await t.restart(e)}(t)";
        let endpoint_only = patch_content(&logged_in_fixture()).unwrap();
        let stale = format!(
            "{endpoint_only}\nfunction later(){{(0,d.getConfig)(d.Config.API_SERVER_URL);{restart}}}"
        );
        fs::write(&path, &stale).unwrap();

        let status = inspect(Some(&path));
        assert!(status.patched);
        assert!(!status.compatible, "{}", status.message);
        assert!(status.message.contains("restore"), "{}", status.message);
    }

    #[test]
    fn session_fallback_uses_the_same_endpoint_and_value_bindings() {
        for (from, to) in [
            ("if(e===m.API_SERVER_URL)", "if(other===m.API_SERVER_URL)"),
            ("void 0!==e", "void 0!==other"),
            ("\"\"!==e", "\"\"!==other"),
            (
                "value:e,source:\"auth_session\"",
                "value:other,source:\"auth_session\"",
            ),
        ] {
            let original = logged_in_fixture().replace(from, to);
            assert!(patch_content(&original).is_err(), "{from}");
        }
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
        assert!(patched.compatible, "{}", patched.message);
        assert!(patched.restore_receipt.is_some());
        let patched_text = String::from_utf8(fs::read(&path).unwrap()).unwrap();
        let old_patched = patched_text.contains("i!==t.apiServerUrl&&await t.restart(i)");
        let new_patched = patched_text.contains(
            "i!==A.LanguageServerClient.getInstance().apiServerUrl&&await A.LanguageServerClient.getInstance().restart(i)",
        );
        let ternary_patched = patched_text
            .contains("i!==t.apiServerUrl?await t.restart(i):await t.whenProcessReplaced()");
        assert!(
            patched_text.contains("\"user_setting\"===n.source")
                && (old_patched || new_patched || ternary_patched),
            "real bundle restart did not prefer an explicit user setting"
        );
        assert!(
            !patched_text.contains("e!==t.apiServerUrl&&await t.restart(e)")
                && !patched_text.contains(
                    "e!==A.LanguageServerClient.getInstance().apiServerUrl&&await A.LanguageServerClient.getInstance().restart(e)"
                )
                && !patched_text.contains(
                    "e!==t.apiServerUrl?await t.restart(e):await t.whenProcessReplaced()"
                ),
            "vendor session restart remained"
        );
        assert_ne!(patched_text.as_bytes(), original.as_slice());

        restore(&receipt).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(inspect(Some(&path)).clean);
    }

    #[test]
    #[ignore = "requires HAXSD_BYOK_DEVIN_GATE_ONLY_EXTENSION pointing at a disposable gate-only copy"]
    fn gate_only_real_extension_is_not_reported_compatible() {
        let path = std::env::var_os("HAXSD_BYOK_DEVIN_GATE_ONLY_EXTENSION")
            .map(PathBuf::from)
            .expect("HAXSD_BYOK_DEVIN_GATE_ONLY_EXTENSION must point at a gate-only copy");
        let status = inspect(Some(&path));
        assert!(status.patched, "{}", status.message);
        assert!(
            !status.compatible,
            "gate-only 3.12.59 must not be compatible: {}",
            status.message
        );
        assert!(
            status.message.contains("needs refresh"),
            "{}",
            status.message
        );
    }
}
