//! Explicit, fail-closed patch boundary for a Devin/Windsurf host extension.
//!
//! This module never discovers an installation path. The caller must provide
//! the exact extension file. It only patches the four stable endpoint anchors
//! observed in the reference app, stores a SHA-256-verified backup, and
//! restores only when the current file still matches the receipt.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const BACKUP_SUFFIX: &str = ".devin-router.backup";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DevinPorts {
    pub api_port: u16,
    pub inference_port: u16,
    pub local_api_port: u16,
}

impl DevinPorts {
    pub fn validate(self) -> Result<Self> {
        if [self.api_port, self.inference_port, self.local_api_port].contains(&0) {
            return Err(Error::Config(
                "Devin host patch ports must be non-zero".into(),
            ));
        }
        if self.api_port == self.inference_port
            || self.api_port == self.local_api_port
            || self.inference_port == self.local_api_port
        {
            return Err(Error::Config(
                "Devin host patch ports must be distinct".into(),
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct PatchParts {
    pub api: bool,
    pub restart: bool,
    pub inference: bool,
    pub local_api: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PatchStatus {
    pub path: PathBuf,
    pub compatible: bool,
    pub clean: bool,
    pub patched: bool,
    pub parts: PatchParts,
    pub ports: Option<DevinPorts>,
    pub backup_path: PathBuf,
    pub backup_available: bool,
    pub current_sha256: String,
    pub backup_sha256: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PatchReceipt {
    pub path: PathBuf,
    pub backup_path: PathBuf,
    pub original_sha256: String,
    pub patched_sha256: String,
    pub ports: DevinPorts,
}

pub fn inspect(path: &Path) -> Result<PatchStatus> {
    let content = fs::read_to_string(path).map_err(|error| {
        Error::Config(format!(
            "cannot read explicit Devin host file {}: {error}",
            path.display()
        ))
    })?;
    inspect_content(path, &content)
}

pub fn apply(path: &Path, ports: DevinPorts) -> Result<PatchReceipt> {
    let ports = ports.validate()?;
    let current = fs::read(path).map_err(|error| {
        Error::Config(format!(
            "cannot read explicit Devin host file {}: {error}",
            path.display()
        ))
    })?;
    let current_content = String::from_utf8(current.clone())
        .map_err(|_| Error::Config("Devin host file is not UTF-8; patch refused".into()))?;
    let status = inspect_content(path, &current_content)?;
    if !status.compatible || !status.clean {
        return Err(Error::Config(refusal_message(&status)));
    }

    let backup_path = backup_path(path);
    ensure_backup(&backup_path, &current)?;
    let patched = patch_content(&current_content, ports)?;
    let patched_bytes = patched.as_bytes();
    atomic_replace(path, patched_bytes)?;
    let patched_status = match inspect(path) {
        Ok(status) if status.patched && status.ports == Some(ports) => status,
        Ok(status) => {
            let _ = atomic_replace(path, &current);
            return Err(Error::Config(format!(
                "Devin host patch verification failed: {}",
                status.message
            )));
        }
        Err(error) => {
            let _ = atomic_replace(path, &current);
            return Err(error);
        }
    };

    Ok(PatchReceipt {
        path: path.to_owned(),
        backup_path,
        original_sha256: sha256(&current),
        patched_sha256: patched_status.current_sha256,
        ports,
    })
}

pub fn restore(receipt: &PatchReceipt) -> Result<()> {
    let current = fs::read(&receipt.path).map_err(Error::Io)?;
    if sha256(&current) != receipt.patched_sha256 {
        return Err(Error::Config(
            "Devin host file changed after patch; restore refused".into(),
        ));
    }
    let backup = fs::read(&receipt.backup_path).map_err(|error| {
        Error::Config(format!(
            "Devin host backup {} is unavailable ({error}); restore refused",
            receipt.backup_path.display()
        ))
    })?;
    if sha256(&backup) != receipt.original_sha256 {
        return Err(Error::Config(
            "Devin host backup SHA-256 mismatch; restore refused".into(),
        ));
    }
    let backup_content = String::from_utf8(backup.clone())
        .map_err(|_| Error::Config("Devin host backup is not UTF-8; restore refused".into()))?;
    let backup_status = inspect_content(&receipt.path, &backup_content)?;
    if !backup_status.clean {
        return Err(Error::Config(
            "Devin host backup is not a recognized clean version; restore refused".into(),
        ));
    }
    atomic_replace(&receipt.path, &backup)?;
    let restored = inspect(&receipt.path)?;
    if !restored.clean {
        let _ = atomic_replace(&receipt.path, &current);
        return Err(Error::Config(
            "Devin host restore verification failed".into(),
        ));
    }
    Ok(())
}

fn inspect_content(path: &Path, content: &str) -> Result<PatchStatus> {
    let parts = PatchParts {
        api: patched_api_regex().is_match(content),
        restart: patched_restart_regex().is_match(content),
        inference: patched_inference_regex().is_match(content),
        local_api: patched_local_regex().is_match(content),
    };
    let ports = patched_ports(content, &parts);
    let patched =
        parts.api && parts.restart && parts.inference && parts.local_api && ports.is_some();
    let clean = clean_anchors_present(content) && !patched;
    let compatible = clean || patched;
    let backup_path = backup_path(path);
    let current_sha256 = sha256(content.as_bytes());
    let (backup_available, backup_sha256) = match fs::read(&backup_path) {
        Ok(bytes) => (true, Some(sha256(&bytes))),
        Err(_) => (false, None),
    };
    let message = if compatible {
        String::new()
    } else if patched || parts.api || parts.restart || parts.inference || parts.local_api {
        "Devin host file has a partial or unknown patch; refusing to modify it".into()
    } else {
        "Devin host file does not match all supported endpoint anchors".into()
    };
    Ok(PatchStatus {
        path: path.to_owned(),
        compatible,
        clean,
        patched,
        parts,
        ports,
        backup_path,
        backup_available,
        current_sha256,
        backup_sha256,
        message,
    })
}

/// Explains why `apply` refused, and what to do instead. An already patched file
/// is the common case on a machine that runs another router, so the message has
/// to name the way out rather than only the refusal.
fn refusal_message(status: &PatchStatus) -> String {
    if status.patched {
        let ports = match status.ports {
            Some(ports) => format!(
                "{} / {} / {}",
                ports.api_port, ports.inference_port, ports.local_api_port
            ),
            None => "unknown ports".into(),
        };
        return format!(
            "Devin host file is already patched (ports {ports}); restore the clean version first, \
             for example the router's own backup next to the file, and then apply this patch"
        );
    }
    if status.compatible {
        return "Devin host file is patched but its ports could not be read; restore the clean \
                version first"
            .into();
    }
    if !status.message.is_empty() {
        return status.message.clone();
    }
    "Devin host file is not a known clean version".into()
}

fn clean_anchors_present(content: &str) -> bool {
    clean_api_regex().is_match(content)
        && clean_restart_regex().is_match(content)
        && clean_inference_regex().is_match(content)
        && clean_local_regex().is_match(content)
}

fn patched_ports(content: &str, parts: &PatchParts) -> Option<DevinPorts> {
    if !(parts.api && parts.restart && parts.inference && parts.local_api) {
        return None;
    }
    let api_port = capture_port(patched_api_regex(), content)?;
    let restart_port = capture_port(patched_restart_regex(), content)?;
    let inference_port = capture_port(patched_inference_regex(), content)?;
    let local_api_port = capture_port(patched_local_regex(), content)?;
    let ports = DevinPorts {
        api_port,
        inference_port,
        local_api_port,
    };
    (api_port == restart_port && ports.validate().is_ok()).then_some(ports)
}

fn capture_port(regex: Regex, content: &str) -> Option<u16> {
    regex
        .captures(content)
        .and_then(|captures| captures.get(1))
        .and_then(|value| value.as_str().parse().ok())
}

fn patch_content(content: &str, ports: DevinPorts) -> Result<String> {
    let api = clean_api_regex();
    let output = replace_once(
        &api,
        content,
        &format!(
            "getApiServerUrlFromContext=A=>{{return\"http://127.0.0.1:{}\"}},e.isStaging",
            ports.api_port
        ),
        "API endpoint anchor",
    )?;
    let restart = clean_restart_regex();
    let output = replace_once(
        &restart,
        &output,
        &format!(
            "async restart(A){{A=\"http://127.0.0.1:{}\",this.apiServerUrl=A",
            ports.api_port
        ),
        "restart endpoint anchor",
    )?;
    let inference = clean_inference_regex();
    let output = replace_once(
        &inference,
        &output,
        &format!("const i=\"http://127.0.0.1:{}\"", ports.inference_port),
        "inference endpoint anchor",
    )?;
    let local = clean_local_regex();
    let output = replace_once_with(
        &local,
        &output,
        |captures| {
            let prefix = captures
                .name("prefix")
                .map(|value| value.as_str())
                .unwrap_or_default();
            let module = captures
                .name("module")
                .map(|value| value.as_str())
                .unwrap_or_default();
            let suffix = captures
                .name("suffix")
                .map(|value| value.as_str())
                .unwrap_or_default();
            format!(
            "{prefix}\"devin-cli\"===this.id?\"http://127.0.0.1:{}\":(0,{module}.getConfig)({module}.Config.API_SERVER_URL){suffix}",
            ports.local_api_port
        )
        },
        "local API endpoint anchor",
    )?;
    Ok(output)
}

fn replace_once(regex: &Regex, content: &str, replacement: &str, name: &str) -> Result<String> {
    replace_once_with(regex, content, |_| replacement.to_owned(), name)
}

fn replace_once_with<F>(regex: &Regex, content: &str, replacement: F, name: &str) -> Result<String>
where
    F: for<'a> Fn(&regex::Captures<'a>) -> String,
{
    if regex.find_iter(content).count() != 1 {
        return Err(Error::Config(format!(
            "Devin host {name} is missing or ambiguous; patch refused"
        )));
    }
    let matched = regex
        .find(content)
        .ok_or_else(|| Error::Config(format!("Devin host {name} is missing; patch refused")))?;
    let captures = regex
        .captures(content)
        .ok_or_else(|| Error::Config(format!("Devin host {name} is missing; patch refused")))?;
    let replacement = replacement(&captures);
    let mut output = String::with_capacity(content.len() + replacement.len());
    output.push_str(&content[..matched.start()]);
    output.push_str(&replacement);
    output.push_str(&content[matched.end()..]);
    Ok(output)
}

fn ensure_backup(path: &Path, source: &[u8]) -> Result<()> {
    if path.exists() {
        let existing = fs::read(path).map_err(Error::Io)?;
        if existing != source {
            return Err(Error::Config(
                "existing Devin host backup does not match the clean file; patch refused".into(),
            ));
        }
        return Ok(());
    }
    atomic_create(path, source)
}

fn atomic_create(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config("Devin host backup has no parent directory".into()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(Error::Io)?;
    temporary.write_all(bytes).map_err(Error::Io)?;
    temporary.flush().map_err(Error::Io)?;
    temporary.as_file().sync_all().map_err(Error::Io)?;
    if temporary.as_file().metadata().map_err(Error::Io)?.len() != bytes.len() as u64 {
        return Err(Error::Config(
            "Devin host backup staging hash mismatch".into(),
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
        .ok_or_else(|| Error::Config("Devin host file has no parent directory".into()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(Error::Io)?;
    temporary.write_all(bytes).map_err(Error::Io)?;
    temporary.flush().map_err(Error::Io)?;
    temporary.as_file().sync_all().map_err(Error::Io)?;
    if temporary.as_file().metadata().map_err(Error::Io)?.len() != bytes.len() as u64 {
        return Err(Error::Config(
            "Devin host replacement staging hash mismatch".into(),
        ));
    }
    let displaced = path.with_extension(format!(
        "{}devin-router-replace",
        path.extension()
            .and_then(|value| value.to_str())
            .map(|value| format!("{value}."))
            .unwrap_or_default()
    ));
    let _ = fs::remove_file(&displaced);
    fs::rename(path, &displaced).map_err(Error::Io)?;
    if let Err(error) = temporary.persist(path).map_err(|error| error.error) {
        let _ = fs::rename(&displaced, path);
        return Err(Error::Io(error));
    }
    fs::remove_file(displaced).map_err(Error::Io)?;
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), BACKUP_SUFFIX))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn clean_api_regex() -> Regex {
    Regex::new(r#"getApiServerUrlFromContext=A=>\{[\s\S]*?\},e\.isStaging"#)
        .expect("valid Devin API anchor")
}

fn clean_restart_regex() -> Regex {
    Regex::new(r#"async restart\(A\)\{this\.apiServerUrl=A"#).expect("valid Devin restart anchor")
}

fn clean_inference_regex() -> Regex {
    Regex::new(r#"const i=\(0,[A-Za-z_$][\w$]*\.getConfig\)\([A-Za-z_$][\w$]*\.Config\.INFERENCE_API_SERVER_URL\)"#).expect("valid Devin inference anchor")
}

fn clean_local_regex() -> Regex {
    Regex::new(r#"(?P<prefix>this\.sendHandshakeRequest\(\{method:"authenticate",params:\{methodId:"windsurf-api-key",_meta:\{api_key:[A-Za-z_$][\w$]*,api_server_url:)\(0,(?P<module>[A-Za-z_$][\w$]*)\.getConfig\)\([A-Za-z_$][\w$]*\.Config\.API_SERVER_URL\)(?P<suffix>\}\}\},[A-Za-z_$][\w$]*\.token\))"#).expect("valid Devin local API anchor")
}

fn patched_api_regex() -> Regex {
    Regex::new(r#"getApiServerUrlFromContext=A=>\{return"http://127\.0\.0\.1:(\d+)"\}"#)
        .expect("valid patched API anchor")
}

fn patched_restart_regex() -> Regex {
    Regex::new(r#"async restart\(A\)\{A="http://127\.0\.0\.1:(\d+)""#)
        .expect("valid patched restart anchor")
}

fn patched_inference_regex() -> Regex {
    Regex::new(r#"const i="http://127\.0\.0\.1:(\d+)""#).expect("valid patched inference anchor")
}

fn patched_local_regex() -> Regex {
    Regex::new(r#"api_server_url:"devin-cli"===this\.id\?"http://127\.0\.0\.1:(\d+)":"#)
        .expect("valid patched local API anchor")
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use tempfile::tempdir;

    use super::*;

    fn ports() -> DevinPorts {
        DevinPorts {
            api_port: 43_110,
            inference_port: 43_111,
            local_api_port: 43_112,
        }
    }

    fn fixture() -> &'static str {
        r#"getApiServerUrlFromContext=A=>{return"https://official"},e.isStaging;async restart(A){this.apiServerUrl=A;const i=(0,X.getConfig)(X.Config.INFERENCE_API_SERVER_URL);this.sendHandshakeRequest({method:"authenticate",params:{methodId:"windsurf-api-key",_meta:{api_key:a,api_server_url:(0,X.getConfig)(X.Config.API_SERVER_URL)}}},t.token)"#
    }

    #[test]
    fn apply_creates_verified_backup_and_restore_returns_clean_fixture() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        let receipt = apply(&path, ports()).unwrap();
        let patched = inspect(&path).unwrap();
        assert!(patched.patched);
        assert_eq!(patched.ports, Some(ports()));
        assert!(PathBuf::from(format!("{}{}", path.display(), BACKUP_SUFFIX)).exists());
        restore(&receipt).unwrap();
        assert!(inspect(&path).unwrap().clean);
        assert_eq!(fs::read_to_string(&path).unwrap(), fixture());
    }

    #[test]
    fn refuses_partial_anchor_without_writing_anything() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        let content = "getApiServerUrlFromContext=A=>{return\"https://official\"},e.isStaging";
        fs::write(&path, content).unwrap();
        assert!(apply(&path, ports()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
        assert!(!path.with_extension("js.devin-router.backup").exists());
    }

    #[test]
    fn refuses_restore_when_backup_hash_changes() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        let receipt = apply(&path, ports()).unwrap();
        fs::write(&receipt.backup_path, b"tampered").unwrap();
        assert!(restore(&receipt).is_err());
        assert!(inspect(&path).unwrap().patched);
    }

    /// A missing backup is an expected user situation, not a crash: the caller
    /// must get a readable configuration error instead of a raw io error.
    #[test]
    fn reports_a_missing_backup_as_a_configuration_error() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        let receipt = apply(&path, ports()).unwrap();
        fs::remove_file(&receipt.backup_path).unwrap();

        let error = restore(&receipt).unwrap_err();
        assert!(
            matches!(error, Error::Config(_)),
            "expected a configuration error, got {error:?}"
        );
        assert!(
            error.to_string().contains("backup"),
            "unexpected message: {error}"
        );
    }

    /// Applying on top of another router's patch is refused, and the refusal has
    /// to show the way out: the ports it found and the need for a clean file.
    #[test]
    fn explains_how_to_proceed_when_the_file_is_already_patched() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();

        let error = apply(&path, ports()).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("43110") && message.contains("43111") && message.contains("43112"),
            "the refusal must name the ports it found: {message}"
        );
        assert!(
            message.contains("restore the clean version"),
            "the refusal must name the way out: {message}"
        );
    }
}
