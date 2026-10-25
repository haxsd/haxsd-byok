//! Explicit, reversible takeover of Devin's user settings.
//!
//! Devin reads `codeium.apiServerUrl` and `codeium.inferenceApiServerUrl`
//! directly from the user settings file. Changing those two values is enough to
//! route the language server, Cascade and the bundled ACP agent through this
//! application without modifying the vendor installation.
//!
//! The file is edited in place as JSONC so comments and unrelated settings
//! survive. A SHA-256-verified backup and receipt are written before the host
//! file changes, and every write is atomic.

use std::{
    fs,
    io::Write,
    ops::Range,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const BACKUP_SUFFIX: &str = ".devin-router.backup";
pub const RECEIPT_SUFFIX: &str = ".devin-router.receipt";
pub const API_SERVER_KEY: &str = "codeium.apiServerUrl";
pub const INFERENCE_API_SERVER_KEY: &str = "codeium.inferenceApiServerUrl";

/// The two Devin endpoints this application owns. The local API listener is
/// still part of the gateway, but Devin receives that URL through the model
/// catalog and the language server derives it from `apiServerUrl`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DevinHostPorts {
    pub api_port: u16,
    pub inference_port: u16,
}

impl DevinHostPorts {
    pub fn validate(self) -> Result<Self> {
        if self.api_port == 0 || self.inference_port == 0 {
            return Err(Error::Config("Devin host ports must be non-zero".into()));
        }
        if self.api_port == self.inference_port {
            return Err(Error::Config("Devin host ports must be distinct".into()));
        }
        Ok(self)
    }

    pub fn api_url(self) -> String {
        format!("http://127.0.0.1:{}", self.api_port)
    }

    pub fn inference_url(self) -> String {
        format!("http://127.0.0.1:{}", self.inference_port)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct HostParts {
    pub api: bool,
    pub inference: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HostStatus {
    pub path: PathBuf,
    /// The settings file exists and is valid JSONC, or it does not exist yet.
    pub compatible: bool,
    /// No Devin endpoint override is present.
    pub unconfigured: bool,
    /// Both endpoint overrides point at loopback HTTP endpoints.
    pub configured: bool,
    pub parts: HostParts,
    pub ports: Option<DevinHostPorts>,
    pub backup_path: PathBuf,
    pub backup_available: bool,
    pub current_sha256: String,
    pub backup_sha256: Option<String>,
    /// Present only while the receipt still describes this exact file, backup
    /// and port pair.
    pub restore_receipt: Option<HostReceipt>,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostReceipt {
    pub path: PathBuf,
    pub backup_path: PathBuf,
    /// `None` means the settings file did not exist before this application
    /// created it, so restore removes the file instead of copying a backup.
    pub original_sha256: Option<String>,
    pub patched_sha256: String,
    pub ports: DevinHostPorts,
}

pub fn inspect(path: &Path) -> Result<HostStatus> {
    recover_displaced(path);
    let current = read_current(path)?;
    let mut status = inspect_current(path, current.as_deref())?;
    status.restore_receipt = usable_receipt(path, &status);
    Ok(status)
}

pub fn apply(path: &Path, ports: DevinHostPorts) -> Result<HostReceipt> {
    recover_displaced(path);
    let ports = ports.validate()?;
    let current = read_current(path)?;
    let status = inspect_current(path, current.as_deref())?;
    if status.configured {
        let detail = status
            .ports
            .map(|ports| format!(" for ports {} / {}", ports.api_port, ports.inference_port))
            .unwrap_or_default();
        return Err(Error::Config(format!(
            "Devin settings already point at a local gateway{detail}; restore the previous values \
             before applying this configuration"
        )));
    }
    if !status.compatible || !status.unconfigured {
        return Err(Error::Config(refusal_message(&status)));
    }

    let original = current.unwrap_or_else(|| b"{\n}\n".to_vec());
    let original_content = std::str::from_utf8(&original)
        .map_err(|_| Error::Config("Devin settings file is not UTF-8; apply refused".into()))?;
    let patched_content = configure_endpoints(original_content, ports)?;
    let patched = patched_content.into_bytes();
    let patched_status = inspect_current(path, Some(&patched))?;
    if !patched_status.configured || patched_status.ports != Some(ports) {
        return Err(Error::Config(
            "Devin settings verification failed before writing; the file was left unchanged".into(),
        ));
    }

    let backup_path = backup_path(path);
    let original_sha256 = if path.is_file() {
        ensure_backup(&backup_path, &original)?;
        Some(sha256(&original))
    } else {
        remove_if_exists(&backup_path)?;
        None
    };
    let receipt = HostReceipt {
        path: path.to_owned(),
        backup_path,
        original_sha256,
        patched_sha256: sha256(&patched),
        ports,
    };
    write_receipt(&receipt)?;
    let write = atomic_replace(path, &patched);
    if let Err(error) = write {
        if !fs::read(path).is_ok_and(|bytes| bytes == patched) {
            discard_failed_receipt(path, &original);
            return Err(error);
        }
    }

    match inspect(path) {
        Ok(verified)
            if verified.configured
                && verified.ports == Some(ports)
                && verified.current_sha256 == receipt.patched_sha256 =>
        {
            Ok(receipt)
        }
        Ok(verified) => {
            let _ = atomic_replace(path, &original);
            discard_failed_receipt(path, &original);
            Err(Error::Config(format!(
                "Devin settings verification failed: {}",
                verified.message
            )))
        }
        Err(error) => {
            let _ = atomic_replace(path, &original);
            discard_failed_receipt(path, &original);
            Err(error)
        }
    }
}

pub fn restore(receipt: &HostReceipt) -> Result<()> {
    let current =
        fs::read(&receipt.path).map_err(|error| host_read_error(&receipt.path, &error))?;
    if sha256(&current) != receipt.patched_sha256 {
        return Err(Error::Config(format!(
            "Devin settings changed after this application configured them; restore refused; \
             the previous file is kept at {}; restore backup: {}",
            receipt.backup_path.display(),
            receipt.backup_path.display()
        )));
    }

    if let Some(original_sha256) = receipt.original_sha256.as_deref() {
        let backup = fs::read(&receipt.backup_path).map_err(|error| {
            Error::Config(format!(
                "Devin settings backup {} is unavailable ({error}); restore refused; restore \
                 backup: {}",
                receipt.backup_path.display(),
                receipt.backup_path.display()
            ))
        })?;
        if sha256(&backup) != original_sha256 {
            return Err(Error::Config(format!(
                "Devin settings backup SHA-256 mismatch; restore refused; restore backup: {}",
                receipt.backup_path.display()
            )));
        }
        atomic_replace(&receipt.path, &backup)?;
    } else {
        fs::remove_file(&receipt.path).map_err(Error::Io)?;
    }

    let _ = fs::remove_file(receipt_path(&receipt.path));
    Ok(())
}

/// Restore a takeover left behind by an unclean exit. No memory receipt is
/// needed: the receipt beside the settings file is the only authority.
pub fn restore_persisted(path: &Path) -> bool {
    let Ok(status) = inspect(path) else {
        return false;
    };
    let Some(receipt) = status.restore_receipt else {
        return false;
    };
    match restore(&receipt) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                %error,
                path = %path.display(),
                "cannot restore Devin settings after an unclean exit"
            );
            false
        }
    }
}

fn read_current(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(host_read_error(path, &error)),
    }
}

fn host_read_error(path: &Path, error: &std::io::Error) -> Error {
    let backup = backup_path(path);
    let hint = backup.is_file().then(|| {
        format!(
            "; a previous file is kept at {}; copy it over {} after closing Devin; restore \
             backup: {}",
            backup.display(),
            path.display(),
            backup.display()
        )
    });
    Error::Config(format!(
        "cannot read Devin settings file {}: {error}; close Devin and check that the file is \
         readable{}",
        path.display(),
        hint.unwrap_or_default()
    ))
}

fn inspect_current(path: &Path, current: Option<&[u8]>) -> Result<HostStatus> {
    let backup_path = backup_path(path);
    let (backup_available, backup_sha256) = match fs::read(&backup_path) {
        Ok(bytes) => (true, Some(sha256(&bytes))),
        Err(_) => (false, None),
    };
    let Some(bytes) = current else {
        return Ok(HostStatus {
            path: path.to_owned(),
            compatible: true,
            unconfigured: true,
            configured: false,
            parts: HostParts::default(),
            ports: None,
            backup_path,
            backup_available,
            current_sha256: String::new(),
            backup_sha256,
            restore_receipt: None,
            message: String::new(),
        });
    };
    let current_sha256 = sha256(bytes);
    let content = match std::str::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => {
            return Ok(invalid_status(
                path,
                backup_path,
                backup_available,
                backup_sha256,
                current_sha256,
                "Devin settings file is not UTF-8; apply and restore are refused",
            ))
        }
    };
    let value: serde_json::Value = match json5::from_str(content) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        Ok(_) => {
            return Ok(invalid_status(
                path,
                backup_path,
                backup_available,
                backup_sha256,
                current_sha256,
                "Devin settings file is not a JSON object; apply and restore are refused",
            ))
        }
        Err(error) => {
            return Ok(invalid_status(
                path,
                backup_path,
                backup_available,
                backup_sha256,
                current_sha256,
                &format!("Devin settings file is not valid JSONC: {error}"),
            ))
        }
    };
    let (comments, structure) = match mask_jsonc(content) {
        Ok(masks) => masks,
        Err(error) => {
            return Ok(invalid_status(
                path,
                backup_path,
                backup_available,
                backup_sha256,
                current_sha256,
                &error.to_string(),
            ))
        }
    };
    for key in [API_SERVER_KEY, INFERENCE_API_SERVER_KEY] {
        if let Err(error) = top_level_value_range(content, &comments, &structure, key) {
            return Ok(invalid_status(
                path,
                backup_path,
                backup_available,
                backup_sha256,
                current_sha256,
                &error.to_string(),
            ));
        }
    }
    let object = value.as_object().expect("JSON object was matched above");
    let api = object.get(API_SERVER_KEY).and_then(|value| value.as_str());
    let inference = object
        .get(INFERENCE_API_SERVER_KEY)
        .and_then(|value| value.as_str());
    let parts = HostParts {
        api: object.contains_key(API_SERVER_KEY),
        inference: object.contains_key(INFERENCE_API_SERVER_KEY),
    };
    let api_port = api.and_then(parse_loopback_port);
    let inference_port = inference.and_then(parse_loopback_port);
    let ports = api_port
        .zip(inference_port)
        .map(|(api_port, inference_port)| DevinHostPorts {
            api_port,
            inference_port,
        })
        .filter(|ports| ports.validate().is_ok());
    let official_defaults = matches!(
        (api, inference),
        (
            Some(
                "https://server.codeium.com"
                    | "https://server-beta.codeium.com"
                    | "https://server.windsurf.com"
                    | "https://server.self-serve.windsurf.com"
            ),
            Some(
                "https://inference.codeium.com"
                    | "https://inference-beta.codeium.com"
                    | "https://inference.windsurf.com"
                    | "https://inference.self-serve.windsurf.com"
            )
        )
    );
    let unconfigured = (!parts.api && !parts.inference) || official_defaults;
    let configured = ports.is_some();
    let compatible = unconfigured || configured;
    let message = if compatible {
        String::new()
    } else if !parts.api || !parts.inference {
        "Devin settings contain only part of the gateway endpoints; set both values or remove \
         them before applying this configuration"
            .into()
    } else {
        "Devin settings already define non-local or invalid API endpoints; restore the previous \
         values before applying this configuration"
            .into()
    };
    Ok(HostStatus {
        path: path.to_owned(),
        compatible,
        unconfigured,
        configured,
        parts,
        ports,
        backup_path,
        backup_available,
        current_sha256,
        backup_sha256,
        restore_receipt: None,
        message,
    })
}

fn invalid_status(
    path: &Path,
    backup_path: PathBuf,
    backup_available: bool,
    backup_sha256: Option<String>,
    current_sha256: String,
    message: &str,
) -> HostStatus {
    HostStatus {
        path: path.to_owned(),
        compatible: false,
        unconfigured: false,
        configured: false,
        parts: HostParts::default(),
        ports: None,
        backup_path,
        backup_available,
        current_sha256,
        backup_sha256,
        restore_receipt: None,
        message: message.into(),
    }
}

fn refusal_message(status: &HostStatus) -> String {
    if !status.message.is_empty() {
        return status.message.clone();
    }
    format!(
        "Devin settings at {} are not in a state this application can configure; close Devin, \
         fix the file, and try again",
        status.path.display()
    )
}

fn usable_receipt(path: &Path, status: &HostStatus) -> Option<HostReceipt> {
    let bytes = fs::read(receipt_path(path)).ok()?;
    let receipt: HostReceipt = serde_json::from_slice(&bytes).ok()?;
    if receipt.path != path || receipt.backup_path != backup_path(path) {
        return None;
    }
    if !status.configured
        || status.ports != Some(receipt.ports)
        || status.current_sha256 != receipt.patched_sha256
    {
        return None;
    }
    match receipt.original_sha256.as_deref() {
        Some(original_sha256) if status.backup_sha256.as_deref() != Some(original_sha256) => None,
        _ => Some(receipt),
    }
}

fn write_receipt(receipt: &HostReceipt) -> Result<()> {
    let path = receipt_path(&receipt.path);
    let bytes = serde_json::to_vec(receipt).map_err(|error| {
        Error::Config(format!(
            "cannot encode the Devin settings restore receipt: {error}"
        ))
    })?;
    atomic_create(&path, &bytes).map_err(|error| {
        Error::Config(format!(
            "cannot persist the Devin settings restore receipt {} ({error}); the file was left \
             unchanged",
            path.display()
        ))
    })
}

fn discard_failed_receipt(path: &Path, original: &[u8]) {
    if (path.is_file() && fs::read(path).is_ok_and(|bytes| bytes == original)) || !path.exists() {
        let _ = fs::remove_file(receipt_path(path));
    }
}

fn parse_loopback_port(value: &str) -> Option<u16> {
    let url = url::Url::parse(value.trim()).ok()?;
    (url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/")
        && url.query().is_none()
        && url.fragment().is_none())
    .then(|| url.port())
    .flatten()
}

fn configure_endpoints(content: &str, ports: DevinHostPorts) -> Result<String> {
    let mut output = content.to_owned();
    for (key, value) in [
        (API_SERVER_KEY, ports.api_url()),
        (INFERENCE_API_SERVER_KEY, ports.inference_url()),
    ] {
        output = set_string_property(&output, key, &value)?;
    }
    json5::from_str::<serde_json::Value>(&output).map_err(|error| {
        Error::Config(format!(
            "Devin settings changed into invalid JSONC while updating endpoints: {error}"
        ))
    })?;
    Ok(output)
}

fn set_string_property(content: &str, key: &str, value: &str) -> Result<String> {
    let (comments, structure) = mask_jsonc(content)?;
    let close = root_close(&structure)?;
    let encoded = serde_json::to_string(value)
        .map_err(|error| Error::Config(format!("cannot encode Devin setting: {error}")))?;
    match top_level_value_range(content, &comments, &structure, key)? {
        Some(range) => {
            let mut output = String::with_capacity(content.len() + encoded.len());
            output.push_str(&content[..range.start]);
            output.push_str(&encoded);
            output.push_str(&content[range.end..]);
            Ok(output)
        }
        None => {
            let mut output = String::with_capacity(content.len() + encoded.len() + key.len() + 12);
            output.push_str(&content[..close]);
            let before = &comments[..close];
            if has_entry(before) {
                let last = before.iter().rev().find(|byte| !byte.is_ascii_whitespace());
                if last != Some(&b',') {
                    output.push(',');
                }
            }
            output.push('\n');
            output.push_str("  \"");
            output.push_str(key);
            output.push_str("\": ");
            output.push_str(&encoded);
            output.push('\n');
            output.push_str(&content[close..]);
            Ok(output)
        }
    }
}

fn top_level_value_range(
    content: &str,
    comments: &[u8],
    structure: &[u8],
    key: &str,
) -> Result<Option<Range<usize>>> {
    let needle = format!("\"{key}\"");
    let mut matches = Vec::new();
    let mut offset = 0;
    while let Some(relative) = find_bytes(&comments[offset..], needle.as_bytes()) {
        let start = offset + relative;
        if depth_at(structure, start) == 1 {
            let mut cursor = start + needle.len();
            skip_mask_space(comments, &mut cursor);
            if comments.get(cursor) == Some(&b':') {
                matches.push((start, cursor + 1));
            }
        }
        offset = start + needle.len();
    }
    match matches.as_slice() {
        [] => Ok(None),
        [(_, colon)] => {
            let mut value_start = *colon;
            skip_mask_space(comments, &mut value_start);
            if comments.get(value_start) != Some(&b'"') {
                return Err(Error::Config(format!(
                    "Devin setting {key} is not a string; restore it manually before applying"
                )));
            }
            let value_end = json_string_end(comments, value_start).ok_or_else(|| {
                Error::Config(format!(
                    "Devin setting {key} has an unterminated string; restore it manually"
                ))
            })?;
            if structure.get(value_start..value_end).is_none()
                || content.get(value_start..value_end).is_none()
            {
                return Err(Error::Config(
                    "Devin settings value offsets are invalid".into(),
                ));
            }
            Ok(Some(value_start..value_end))
        }
        _ => Err(Error::Config(format!(
            "Devin setting {key} appears more than once at the top level; restore it manually"
        ))),
    }
}

fn mask_jsonc(content: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let bytes = content.as_bytes();
    let mut comments = bytes.to_vec();
    let mut structure = bytes.to_vec();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            comments[index] = b'"';
            structure[index] = b' ';
            index += 1;
            let mut escaped = false;
            while index < bytes.len() {
                let byte = bytes[index];
                structure[index] = b' ';
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    comments[index] = b'"';
                    structure[index] = b' ';
                    index += 1;
                    break;
                }
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
            while index < bytes.len() {
                let byte = bytes[index];
                if byte == b'\n' {
                    index += 1;
                    break;
                }
                comments[index] = b' ';
                structure[index] = b' ';
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            comments[index] = b' ';
            structure[index] = b' ';
            index += 1;
            while index < bytes.len() {
                let byte = bytes[index];
                if byte == b'\n' {
                    index += 1;
                    continue;
                }
                comments[index] = b' ';
                structure[index] = b' ';
                if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    comments[index + 1] = b' ';
                    structure[index + 1] = b' ';
                    index += 2;
                    break;
                }
                index += 1;
            }
            continue;
        }
        index += 1;
    }
    let first = structure
        .iter()
        .position(|byte| *byte == b'{')
        .ok_or_else(|| {
            Error::Config(
                "Devin settings file is not a top-level JSON object; apply refused".into(),
            )
        })?;
    if structure[..first]
        .iter()
        .any(|byte| !byte.is_ascii_whitespace())
    {
        return Err(Error::Config(
            "Devin settings file has content before its top-level object; apply refused".into(),
        ));
    }
    Ok((comments, structure))
}

fn root_close(structure: &[u8]) -> Result<usize> {
    let open = structure
        .iter()
        .position(|byte| *byte == b'{')
        .ok_or_else(|| Error::Config("Devin settings file has no top-level object".into()))?;
    let mut depth = 0_i32;
    for (index, byte) in structure.iter().enumerate().skip(open) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if structure[index + 1..]
                        .iter()
                        .any(|byte| !byte.is_ascii_whitespace())
                    {
                        return Err(Error::Config(
                            "Devin settings file has content after its top-level object; apply \
                             refused"
                                .into(),
                        ));
                    }
                    return Ok(index);
                }
            }
            _ => {}
        }
    }
    Err(Error::Config(
        "Devin settings file has an unterminated top-level object; apply refused".into(),
    ))
}

fn depth_at(structure: &[u8], position: usize) -> i32 {
    let mut depth = 0_i32;
    for byte in &structure[..position] {
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
    }
    depth
}

fn json_string_end(mask: &[u8], start: usize) -> Option<usize> {
    if mask.get(start) != Some(&b'"') {
        return None;
    }
    let mut index = start + 1;
    let mut escaped = false;
    while index < mask.len() {
        let byte = mask[index];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return Some(index + 1);
        }
        index += 1;
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn skip_mask_space(mask: &[u8], cursor: &mut usize) {
    while mask
        .get(*cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        *cursor += 1;
    }
}

fn has_entry(mask: &[u8]) -> bool {
    mask.iter()
        .any(|byte| !byte.is_ascii_whitespace() && *byte != b',' && *byte != b'{')
}

fn ensure_backup(path: &Path, source: &[u8]) -> Result<()> {
    if fs::read(path).is_ok_and(|existing| existing == source) {
        return Ok(());
    }
    atomic_create(path, source)
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

fn atomic_create(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::Config("Devin settings or backup path has no parent directory".into())
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(Error::Io)?;
    temporary.write_all(bytes).map_err(Error::Io)?;
    temporary.flush().map_err(Error::Io)?;
    temporary.as_file().sync_all().map_err(Error::Io)?;
    if temporary.as_file().metadata().map_err(Error::Io)?.len() != bytes.len() as u64 {
        return Err(Error::Config("Devin settings staging hash mismatch".into()));
    }
    temporary
        .persist(path)
        .map_err(|error| Error::Io(error.error))?;
    Ok(())
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config("Devin settings file has no parent directory".into()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(Error::Io)?;
    temporary.write_all(bytes).map_err(Error::Io)?;
    temporary.flush().map_err(Error::Io)?;
    temporary.as_file().sync_all().map_err(Error::Io)?;
    if temporary.as_file().metadata().map_err(Error::Io)?.len() != bytes.len() as u64 {
        return Err(Error::Config(
            "Devin settings replacement staging hash mismatch".into(),
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
            "cannot put back the displaced Devin settings file"
        );
    }
}

fn displaced_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "{}devin-router-replace",
        path.extension()
            .and_then(|value| value.to_str())
            .map(|value| format!("{value}."))
            .unwrap_or_default()
    ))
}

fn backup_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), BACKUP_SUFFIX))
}

fn receipt_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), RECEIPT_SUFFIX))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports() -> DevinHostPorts {
        DevinHostPorts {
            api_port: 43_110,
            inference_port: 43_111,
        }
    }

    fn settings_file(directory: &tempfile::TempDir, content: Option<&str>) -> PathBuf {
        let path = directory.path().join("settings.json");
        if let Some(content) = content {
            fs::write(&path, content).unwrap();
        }
        path
    }

    #[test]
    fn absent_file_is_created_configured_and_restored_to_absence() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_file(&directory, None);

        let receipt = apply(&path, ports()).unwrap();
        let status = inspect(&path).unwrap();
        assert!(status.configured);
        assert_eq!(status.ports, Some(ports()));
        assert!(status.restore_receipt.is_some());
        assert!(!receipt.backup_path.exists());

        restore(&receipt).unwrap();
        assert!(!path.exists());
        assert!(!receipt_path(&path).exists());
    }

    #[test]
    fn comments_and_unrelated_settings_survive() {
        let directory = tempfile::tempdir().unwrap();
        let original = r#"{
  // keep this comment
  "editor.fontSize": 15,
  "devin.acp.enabledAgents": {
    "devin-cli": true
  }
}
"#;
        let path = settings_file(&directory, Some(original));

        let receipt = apply(&path, ports()).unwrap();
        let patched = fs::read_to_string(&path).unwrap();
        assert!(patched.contains("// keep this comment"));
        assert!(patched.contains("\"editor.fontSize\": 15"));
        assert!(patched.contains("\"devin-cli\": true"));
        assert!(patched.contains("\"codeium.apiServerUrl\": \"http://127.0.0.1:43110\""));
        assert!(patched.contains("\"codeium.inferenceApiServerUrl\": \"http://127.0.0.1:43111\""));
        assert_eq!(
            json5::from_str::<serde_json::Value>(&patched).unwrap()["editor.fontSize"],
            serde_json::json!(15)
        );

        restore(&receipt).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn existing_values_are_replaced_without_duplicate_keys() {
        let directory = tempfile::tempdir().unwrap();
        let original = r#"{
  "codeium.apiServerUrl": "https://server.codeium.com",
  "codeium.inferenceApiServerUrl": "https://inference.codeium.com",
  "window.zoomLevel": 1
}
"#;
        let path = settings_file(&directory, Some(original));
        apply(&path, ports()).unwrap();
        let patched = fs::read_to_string(&path).unwrap();
        assert_eq!(patched.matches("\"codeium.apiServerUrl\"").count(), 1);
        assert_eq!(
            patched.matches("\"codeium.inferenceApiServerUrl\"").count(),
            1
        );
        assert_eq!(inspect(&path).unwrap().ports, Some(ports()));
    }

    #[test]
    fn partial_or_foreign_endpoints_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        for content in [
            r#"{"codeium.apiServerUrl":"http://127.0.0.1:9000"}"#,
            r#"{
                "codeium.apiServerUrl":"http://127.0.0.1:9000",
                "codeium.inferenceApiServerUrl":"http://127.0.0.1:9001"
            }"#,
            r#"{
                "codeium.apiServerUrl":"https://server.example",
                "codeium.inferenceApiServerUrl":"https://inference.example"
            }"#,
        ] {
            let path = settings_file(&directory, Some(content));
            assert!(apply(&path, ports()).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
        }
    }

    #[test]
    fn duplicate_top_level_endpoint_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let content = r#"{
          "codeium.apiServerUrl": "https://a",
          "codeium.apiServerUrl": "https://b"
        }"#;
        let path = settings_file(&directory, Some(content));
        let error = apply(&path, ports()).unwrap_err().to_string();
        assert!(error.contains("more than once"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn changed_settings_are_not_overwritten_by_restore() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_file(&directory, Some("{\n}\n"));
        let receipt = apply(&path, ports()).unwrap();
        let changed = fs::read_to_string(&path).unwrap().replace("43110", "43120");
        fs::write(&path, &changed).unwrap();

        let error = restore(&receipt).unwrap_err().to_string();
        assert!(error.contains("restore refused"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), changed);
    }

    #[test]
    fn a_copied_or_tampered_receipt_is_not_usable() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first-settings.json");
        let second = directory.path().join("second-settings.json");
        fs::write(&first, "{\n}\n").unwrap();
        fs::write(&second, "{\n}\n").unwrap();
        apply(&first, ports()).unwrap();
        apply(&second, ports()).unwrap();

        fs::copy(receipt_path(&first), receipt_path(&second)).unwrap();
        assert!(inspect(&second).unwrap().restore_receipt.is_none());
    }

    #[test]
    fn non_loopback_https_and_paths_are_not_mistaken_for_the_gateway() {
        assert_eq!(parse_loopback_port("http://127.0.0.1:43110"), Some(43110));
        for value in [
            "https://127.0.0.1:43110",
            "http://localhost:43110",
            "http://127.0.0.1:43110/other",
            "http://user@127.0.0.1:43110",
            "http://127.0.0.1:43110?x=1",
        ] {
            assert_eq!(parse_loopback_port(value), None, "{value}");
        }
    }
}
