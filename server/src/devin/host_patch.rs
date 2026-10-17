//! Explicit, fail-closed patch boundary for a Devin/Windsurf host extension.
//!
//! This module never discovers an installation path. The caller must provide
//! the exact extension file. It only patches the four stable endpoint anchors
//! observed in the reference app, stores a SHA-256-verified backup, and writes
//! the original patch receipt next to the file before touching it, so a later
//! process can restore without holding anything in memory.

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
pub const RECEIPT_SUFFIX: &str = ".devin-router.receipt";

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
    /// 落盘的原始 apply 收据，仅当它仍然描述当前文件、备份与端口时才有值。
    /// 缺失或被改动过就是 `None`：恢复只能用它，不能从现有哈希反推。
    pub restore_receipt: Option<PatchReceipt>,
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
    recover_displaced(path);
    let content = fs::read_to_string(path).map_err(|error| host_read_error(path, &error))?;
    let mut status = inspect_content(path, &content)?;
    status.restore_receipt = usable_receipt(path, &status);
    Ok(status)
}

/// 宿主文件读不出来时的说法。
///
/// 这句话会直接进界面提示，所以它指认是哪个文件，并给出可检查的一步；`os error 5`
/// 这类系统原文跟在后面，是给用户提交问题时用的证据。
///
/// "cannot read explicit Devin host file" 是界面的匹配片段（`apps/desktop` 的
/// `harnessErrorText.ts` 按它换成中文），改文案要连着那张表一起改。
fn host_read_error(path: &Path, error: &std::io::Error) -> Error {
    // 宿主文件读不出来（被移走、删掉、权限不对）时，如果旁边还留着上一次 apply 的干净
    // 备份，用户唯一能照做的一步就是"把备份复制回去"。这句话对 status/apply/restore
    // 三个入口都成立——以前只有收据校验之后的分支才给指引，而那条路实际走不到
    // （复核 12-devin/P3-2）。
    let backup = backup_path(path);
    let hint = fs::read(&backup).ok().map(|_| {
        format!(
            "; if this file was patched by this app, restore it by copying {} over {} and \
             restarting the Devin client; backup: {}",
            backup.display(),
            path.display(),
            backup.display()
        )
    });
    Error::Config(format!(
        "cannot read explicit Devin host file {}: {error}; check that the file still exists and \
         is readable{}",
        path.display(),
        hint.unwrap_or_default()
    ))
}

/// 还原被拒绝时的说法。
///
/// 每一支都带两样东西：备份在哪（句末固定的 `backup: <path>`，界面按它把路径留在提示里）
/// 和用户下一步该做什么。以前这里只说 "restore refused"，用户手上既有收据又有备份，却
/// 不知道下一步该做什么。
fn restore_refusal(receipt: &PatchReceipt, reason: &str) -> Error {
    let backup_usable =
        fs::read(&receipt.backup_path).is_ok_and(|bytes| sha256(&bytes) == receipt.original_sha256);
    if backup_usable {
        Error::Config(format!(
            "{reason}; restore manually by copying {} over {} and restarting the Devin client; \
             backup: {}",
            receipt.backup_path.display(),
            receipt.path.display(),
            receipt.backup_path.display()
        ))
    } else {
        Error::Config(format!(
            "{reason}; backup {} is unavailable or does not match the receipt, so restore the \
             clean version by reinstalling the Devin client; backup: {}",
            receipt.backup_path.display(),
            receipt.backup_path.display()
        ))
    }
}

pub fn apply(path: &Path, ports: DevinPorts) -> Result<PatchReceipt> {
    recover_displaced(path);
    let ports = ports.validate()?;
    let current = fs::read(path).map_err(|error| host_read_error(path, &error))?;
    let current_content = String::from_utf8(current.clone())
        .map_err(|_| Error::Config("Devin host file is not UTF-8; patch refused".into()))?;
    let status = inspect_content(path, &current_content)?;
    if !status.compatible || !status.clean {
        return Err(Error::Config(refusal_message(&status)));
    }

    let backup_path = backup_path(path);
    ensure_backup(&backup_path, &current)?;
    let patched = patch_content(&current_content, ports)?;
    let receipt = PatchReceipt {
        path: path.to_owned(),
        backup_path,
        original_sha256: sha256(&current),
        patched_sha256: sha256(patched.as_bytes()),
        ports,
    };
    // 收据先于宿主改动落盘：这一步之后无论哪里失败，磁盘上都还有恢复凭据。
    write_receipt(&receipt)?;
    if let Err(error) = atomic_replace(path, patched.as_bytes()) {
        // 替换的尾部动作（清掉离位副本）失败时，补丁其实已经落到宿主文件上了：
        // 成败按宿主文件的实际内容判，别让"应用失败"和实际状态对不上。
        if !fs::read(path).is_ok_and(|bytes| bytes == patched.as_bytes()) {
            discard_failed_receipt(path, &current);
            return Err(error);
        }
    }
    let verified = fs::read_to_string(path)
        .map_err(Error::Io)
        .and_then(|content| inspect_content(path, &content));
    match verified {
        Ok(status)
            if status.patched
                && status.ports == Some(ports)
                && status.current_sha256 == receipt.patched_sha256 =>
        {
            Ok(receipt)
        }
        Ok(status) => {
            let _ = atomic_replace(path, &current);
            discard_failed_receipt(path, &current);
            Err(Error::Config(format!(
                "Devin host patch verification failed: {}",
                status.message
            )))
        }
        Err(error) => {
            let _ = atomic_replace(path, &current);
            discard_failed_receipt(path, &current);
            Err(error)
        }
    }
}

pub fn restore(receipt: &PatchReceipt) -> Result<()> {
    let current =
        fs::read(&receipt.path).map_err(|error| host_read_error(&receipt.path, &error))?;
    if sha256(&current) != receipt.patched_sha256 {
        return Err(restore_refusal(
            receipt,
            "Devin host file changed after patch; restore refused",
        ));
    }
    let backup = fs::read(&receipt.backup_path).map_err(|error| {
        Error::Config(format!(
            "Devin host backup {} is unavailable ({error}); restore refused; restore the clean \
             version by reinstalling the Devin client; backup: {}",
            receipt.backup_path.display(),
            receipt.backup_path.display()
        ))
    })?;
    if sha256(&backup) != receipt.original_sha256 {
        return Err(restore_refusal(
            receipt,
            "Devin host backup SHA-256 mismatch; restore refused",
        ));
    }
    let backup_content = String::from_utf8(backup.clone()).map_err(|_| {
        Error::Config(format!(
            "Devin host backup is not UTF-8; restore refused; backup: {}",
            receipt.backup_path.display()
        ))
    })?;
    let backup_status = inspect_content(&receipt.path, &backup_content)?;
    if !backup_status.clean {
        return Err(Error::Config(format!(
            "Devin host backup is not a recognized clean version; restore refused; backup: {}",
            receipt.backup_path.display()
        )));
    }
    atomic_replace(&receipt.path, &backup)?;
    let restored = inspect(&receipt.path)?;
    if !restored.clean {
        let _ = atomic_replace(&receipt.path, &current);
        return Err(Error::Config(
            "Devin host restore verification failed".into(),
        ));
    }
    // 恢复成功后收据失效：文件已回到原文，收据描述的补丁版本不复存在。
    let _ = fs::remove_file(receipt_path(&receipt.path));
    Ok(())
}

/// 强杀收尾用的恢复入口：调用它的进程没有内存里的收据，只能靠宿主文件旁边落盘的
/// 那份（`restore` 的语义完全一致：收据、备份、端口任一不符就拒绝）。
///
/// 优雅退出**不**走这条路：`docs/devin-go-live.md` 把"退出 Devin 后仍在本机网关
/// 跑、用界面按钮恢复原文件"写成产品步骤，退出时自动还原会让那一步失效。只有进程
/// 被强杀、宿主文件留在补丁状态而网关已经不在时才需要它。
///
/// 返回是否真的恢复了。收尾助手不该因为"没什么可恢复"而失败，所以读不出来、没有
/// 可用收据、恢复动作本身失败都归为 `false`（失败原因进日志）。
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
                "cannot restore the Devin host file after an unclean exit"
            );
            false
        }
    }
}

/// The receipt only counts while it still describes this exact host file, its
/// backup, and its ports. A replaced host file, a changed or missing backup, a
/// receipt copied from another installation, or a backup that is no longer a
/// recognized clean version all mean "no restore offered" — never a receipt
/// reconstructed from the hashes at hand.
fn usable_receipt(path: &Path, status: &PatchStatus) -> Option<PatchReceipt> {
    let bytes = fs::read(receipt_path(path)).ok()?;
    let receipt: PatchReceipt = serde_json::from_slice(&bytes).ok()?;
    if receipt.path != path || receipt.backup_path != backup_path(path) {
        return None;
    }
    if !status.patched
        || status.ports != Some(receipt.ports)
        || status.current_sha256 != receipt.patched_sha256
    {
        return None;
    }
    if status.backup_sha256.as_deref() != Some(receipt.original_sha256.as_str()) {
        return None;
    }
    let backup = fs::read_to_string(&receipt.backup_path).ok()?;
    clean_anchors_present(&backup).then_some(receipt)
}

/// Persists the receipt before the host file changes; a failure here means the
/// patch never lands, so nothing has to be recovered.
fn write_receipt(receipt: &PatchReceipt) -> Result<()> {
    let path = receipt_path(&receipt.path);
    let bytes = serde_json::to_vec(receipt).map_err(|error| {
        Error::Config(format!(
            "cannot encode the Devin host restore receipt: {error}"
        ))
    })?;
    atomic_create(&path, &bytes).map_err(|error| {
        Error::Config(format!(
            "cannot persist the Devin host restore receipt {} ({error}); the host file was left \
             unchanged",
            path.display()
        ))
    })
}

/// A patch that never reached the host file leaves no receipt behind. The
/// receipt is dropped only once the host file is known to be the original
/// again; otherwise it is the only record of how to go back.
fn discard_failed_receipt(path: &Path, original: &[u8]) {
    if fs::read(path).is_ok_and(|bytes| bytes == original) {
        let _ = fs::remove_file(receipt_path(path));
    }
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
        restore_receipt: None,
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
        // 有备份就把路径写出来：用户要手动还原时，第一步就是找到这份文件。句末固定带
        // `backup: <path>`，界面（apps/desktop 的 harnessErrorText.ts）按它把路径留下。
        let backup = if status.backup_available {
            format!(
                "; the verified clean copy is at {}; copy it over this file and restart the \
                 Devin client",
                status.backup_path.display()
            )
        } else {
            String::new()
        };
        return format!(
            "Devin host file is already patched (ports {ports}); restore the clean version first\
             {backup}, then apply this patch; backup: {}",
            status.backup_path.display()
        );
    }
    if status.compatible {
        return format!(
            "Devin host file is patched but its ports could not be read; restore the clean \
             version by reinstalling the Devin client; backup: {}",
            status.backup_path.display()
        );
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

/// 备份的写入：内容与宿主文件一致就复用，不一致就轮换成新的。
///
/// 调用方（`apply`）已经确认宿主文件是**已知干净版本**，所以不匹配的旧备份只可能来自
/// 更早的客户端版本。以前这里永久拒绝打补丁，用户只能自己去文件系统里找那个备份删掉，
/// 而提示里连备份路径都没给。
fn ensure_backup(path: &Path, source: &[u8]) -> Result<()> {
    if let Ok(existing) = fs::read(path) {
        if existing == source {
            return Ok(());
        }
        tracing::warn!(
            path = %path.display(),
            "replacing a Devin host backup that does not match the current clean file"
        );
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
    let displaced = displaced_path(path);
    let _ = fs::remove_file(&displaced);
    fs::rename(path, &displaced).map_err(Error::Io)?;
    if let Err(error) = temporary.persist(path).map_err(|error| error.error) {
        // 新内容没落上就把离位副本放回去，宿主文件保持原样。
        let _ = fs::rename(&displaced, path);
        return Err(Error::Io(error));
    }
    // 到这里宿主文件已经是新内容，这次替换就算成功：离位副本清不掉只是留下一份
    // 垃圾，以前把它当成错误返回，调用方会以为补丁没生效。
    let _ = fs::remove_file(&displaced);
    Ok(())
}

/// 替换过程中"宿主文件已被改名离位、新内容还没放上"的那份副本。
fn displaced_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "{}devin-router-replace",
        path.extension()
            .and_then(|value| value.to_str())
            .map(|value| format!("{value}."))
            .unwrap_or_default()
    ))
}

/// 上次替换在离位与落盘之间中断（崩溃、强杀）时，宿主文件不在原位、只剩离位副本，
/// 客户端直接报文件缺失。这种残留只可能由 `atomic_replace` 自己造成，所以下一次
/// 读或写宿主文件之前先把它放回去。
fn recover_displaced(path: &Path) {
    let displaced = displaced_path(path);
    if path.exists() || !displaced.is_file() {
        return;
    }
    if let Err(error) = fs::rename(&displaced, path) {
        tracing::warn!(
            %error,
            path = %path.display(),
            "cannot put back the displaced Devin host file"
        );
    }
}

fn backup_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), BACKUP_SUFFIX))
}

/// 收据与宿主文件同目录、名字可预测，重启后不需要索引就能找到。
fn receipt_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), RECEIPT_SUFFIX))
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

/// 宿主样例与端口由 host_patch 与 control 的测试共用：两处锚点各自漂移过一次，
/// 再分头维护就会让其中一侧静默地测不到真实文件。
#[cfg(test)]
pub(crate) mod test_fixture {
    use super::DevinPorts;

    pub(crate) const HOST_FILE: &str = r#"getApiServerUrlFromContext=A=>{return"https://official"},e.isStaging;async restart(A){this.apiServerUrl=A;const i=(0,X.getConfig)(X.Config.INFERENCE_API_SERVER_URL);this.sendHandshakeRequest({method:"authenticate",params:{methodId:"windsurf-api-key",_meta:{api_key:a,api_server_url:(0,X.getConfig)(X.Config.API_SERVER_URL)}}},t.token)"#;

    pub(crate) fn ports() -> DevinPorts {
        DevinPorts {
            api_port: 43_110,
            inference_port: 43_111,
            local_api_port: 43_112,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use tempfile::tempdir;

    use super::*;

    fn ports() -> DevinPorts {
        test_fixture::ports()
    }

    fn fixture() -> &'static str {
        test_fixture::HOST_FILE
    }

    /// 收据旁挂文件的位置写死在这里：实现改了文件名，测试必须跟着红。
    fn receipt_file(path: &Path) -> PathBuf {
        PathBuf::from(format!("{}{}", path.display(), ".devin-router.receipt"))
    }

    /// 从状态 JSON 里取恢复收据，而不是从 Rust 字段取：进程重启后页面拿到的
    /// 只能是序列化结果，缺字段必须表现为可取到但为空。
    fn restore_receipt_of(status: &PatchStatus) -> serde_json::Value {
        let status = serde_json::to_value(status).unwrap();
        status["restore_receipt"].clone()
    }

    /// apply 的返回值在进程重启后就没了；inspect 只能靠落盘的原始收据提供恢复。
    #[test]
    fn inspect_offers_the_persisted_receipt_after_the_apply_result_is_gone() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();

        let value = restore_receipt_of(&inspect(&path).unwrap());
        let receipt: PatchReceipt = serde_json::from_value(value)
            .expect("inspect must offer the receipt that was persisted before the patch");
        restore(&receipt).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), fixture());
    }

    /// 收据丢了就是丢了：不许用当前文件哈希和备份反推一份出来。
    #[test]
    fn a_missing_receipt_is_not_rebuilt_from_the_patched_file_or_backup() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();
        fs::remove_file(receipt_file(&path)).unwrap();

        let status = serde_json::to_value(inspect(&path).unwrap()).unwrap();
        assert!(
            matches!(status.get("restore_receipt"), Some(value) if value.is_null()),
            "a deleted receipt must stay empty instead of being rebuilt from hashes: {status}"
        );
        assert!(inspect(&path).unwrap().patched);
    }

    /// 宿主文件在补丁之后被改过，收据就不再描述它，恢复入口必须消失，改动原样保留。
    #[test]
    fn a_host_file_changed_after_the_patch_invalidates_the_receipt() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();
        let edited = format!("{}// user edit\n", fs::read_to_string(&path).unwrap());
        fs::write(&path, &edited).unwrap();

        let status = serde_json::to_value(inspect(&path).unwrap()).unwrap();
        assert!(
            matches!(status.get("restore_receipt"), Some(value) if value.is_null()),
            "a host file changed after the patch must not keep a usable receipt: {status}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), edited);
    }

    /// 备份被改过时收据同样失效；宿主文件不能被恢复流程碰掉。
    #[test]
    fn a_changed_backup_invalidates_the_receipt() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();
        let patched = fs::read_to_string(&path).unwrap();
        fs::write(backup_path(&path), b"tampered").unwrap();

        let status = serde_json::to_value(inspect(&path).unwrap()).unwrap();
        assert!(
            matches!(status.get("restore_receipt"), Some(value) if value.is_null()),
            "a changed backup must not keep a usable receipt: {status}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), patched);
    }

    /// 收据只对它写下的那个宿主文件有效：把 A 的收据放到 B 旁边，或者把 B 自己的
    /// 收据改成指向 A，都必须拒绝，哪怕哈希和端口全部对得上。
    #[test]
    fn a_receipt_written_for_another_host_file_is_refused() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first-extension.js");
        let second = directory.path().join("second-extension.js");
        fs::write(&first, fixture()).unwrap();
        fs::write(&second, fixture()).unwrap();
        apply(&first, ports()).unwrap();
        apply(&second, ports()).unwrap();

        fs::remove_file(receipt_file(&second)).unwrap();
        fs::copy(receipt_file(&first), receipt_file(&second)).unwrap();
        let status = serde_json::to_value(inspect(&second).unwrap()).unwrap();
        assert!(
            matches!(status.get("restore_receipt"), Some(value) if value.is_null()),
            "a receipt copied from another installation must not be usable: {status}"
        );

        fs::copy(receipt_file(&first), receipt_file(&second)).unwrap();
        let mut receipt: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(receipt_file(&second)).unwrap()).unwrap();
        receipt["path"] = serde_json::json!(first.to_str().unwrap());
        receipt["backup_path"] = serde_json::json!(backup_path(&second).to_str().unwrap());
        fs::write(receipt_file(&second), serde_json::to_vec(&receipt).unwrap()).unwrap();
        let status = serde_json::to_value(inspect(&second).unwrap()).unwrap();
        assert!(
            matches!(status.get("restore_receipt"), Some(value) if value.is_null()),
            "a receipt naming another host file must not be usable: {status}"
        );
    }

    /// 收据被改过：端口必须和宿主文件里实际写着的端口一致。
    #[test]
    fn a_receipt_with_other_ports_than_the_patched_file_is_refused() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();
        let mut receipt: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(receipt_file(&path)).unwrap()).unwrap();
        receipt["ports"]["api_port"] = serde_json::json!(43_120);
        fs::write(receipt_file(&path), serde_json::to_vec(&receipt).unwrap()).unwrap();

        let status = serde_json::to_value(inspect(&path).unwrap()).unwrap();
        assert!(
            matches!(status.get("restore_receipt"), Some(value) if value.is_null()),
            "the receipt must agree with the ports written into the host file: {status}"
        );
    }

    /// 收据可以连同备份一起被改写：备份本身仍必须是可识别的干净版本。
    #[test]
    fn a_receipt_pointing_at_a_non_clean_backup_is_refused() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();
        let junk = "not a Devin host file";
        fs::write(backup_path(&path), junk).unwrap();
        let mut receipt: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(receipt_file(&path)).unwrap()).unwrap();
        receipt["original_sha256"] = serde_json::json!(sha256(junk.as_bytes()));
        fs::write(receipt_file(&path), serde_json::to_vec(&receipt).unwrap()).unwrap();

        let status = serde_json::to_value(inspect(&path).unwrap()).unwrap();
        assert!(
            matches!(status.get("restore_receipt"), Some(value) if value.is_null()),
            "a backup that is not a recognised clean file must not be restorable: {status}"
        );
    }

    /// 收据写不下去就不能改宿主：宁可这次补丁失败，也不留下没有凭据的新补丁。
    #[test]
    fn a_receipt_that_cannot_be_written_leaves_the_host_file_untouched() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        fs::create_dir(receipt_file(&path)).unwrap();

        let error = apply(&path, ports()).unwrap_err();
        assert!(
            error.to_string().contains("receipt"),
            "the refusal has to name the receipt it could not write: {error}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), fixture());
        assert!(!inspect(&path).unwrap().patched);
    }

    /// 恢复之后收据必须失效，备份留给下一次 apply。
    #[test]
    fn restore_clears_the_persisted_receipt_and_keeps_the_backup() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();
        let receipt: PatchReceipt =
            serde_json::from_value(restore_receipt_of(&inspect(&path).unwrap())).unwrap();

        restore(&receipt).unwrap();
        assert!(
            !receipt_file(&path).exists(),
            "an applied receipt must not survive the restore"
        );
        assert!(
            receipt.backup_path.exists(),
            "the backup stays for the next apply"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), fixture());
    }

    /// 恢复之后再次 apply 必须重新落一份可用收据。
    #[test]
    fn apply_after_restore_persists_a_usable_receipt_again() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();
        let first: PatchReceipt =
            serde_json::from_value(restore_receipt_of(&inspect(&path).unwrap())).unwrap();
        restore(&first).unwrap();

        apply(&path, ports()).unwrap();
        let second: PatchReceipt =
            serde_json::from_value(restore_receipt_of(&inspect(&path).unwrap())).unwrap();
        restore(&second).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), fixture());
    }

    /// 用户自己把备份拷回宿主文件时不会顺手删收据；这份失效收据不能挡住下一次 apply。
    #[test]
    fn a_stale_receipt_left_by_a_manual_restore_does_not_block_apply() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();
        fs::copy(backup_path(&path), &path).unwrap();
        assert!(inspect(&path).unwrap().restore_receipt.is_none());

        apply(&path, ports()).unwrap();
        let receipt: PatchReceipt =
            serde_json::from_value(restore_receipt_of(&inspect(&path).unwrap())).unwrap();
        restore(&receipt).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), fixture());
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

    /// 宿主文件被换过时还原会被拒绝，但备份还在：提示必须给出"复制备份回去"这一步，
    /// 否则用户手上同时有收据和备份，却只能看到一句 restore refused。
    #[test]
    fn a_changed_host_file_tells_the_user_how_to_restore_manually() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        let receipt = apply(&path, ports()).unwrap();
        fs::write(&path, "// someone else edited the host file").unwrap();

        let error = restore(&receipt).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains(&receipt.backup_path.display().to_string()),
            "提示里要有备份路径: {message}"
        );
        assert!(
            message.contains(&path.display().to_string()),
            "提示里要有宿主文件路径: {message}"
        );
        assert!(
            message.contains("copying"),
            "提示里要给出可照做的一步: {message}"
        );
    }

    /// 客户端升级后残留的旧备份不该让打补丁永久失败：当前文件已经是已知干净版本，
    /// 对不上的旧备份只说明它来自更早的版本，按过期轮换掉即可。
    #[test]
    fn a_stale_backup_from_an_older_client_is_rotated_instead_of_refusing() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        let receipt = apply(&path, ports()).unwrap();
        restore(&receipt).unwrap();
        // 旧客户端留下的备份：既不是当前文件，也不是本次收据记录的原文。
        fs::write(
            &receipt.backup_path,
            b"/* stale backup from an older client */",
        )
        .unwrap();

        let rotated =
            apply(&path, ports()).expect("干净文件 + 过期备份必须还能打补丁，而不是永久拒绝");
        assert_eq!(
            fs::read_to_string(&rotated.backup_path).unwrap(),
            fixture(),
            "过期备份必须被轮换成当前干净文件"
        );
        restore(&rotated).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), fixture());
    }

    /// 宿主文件不在了（客户端被移动或卸载，路径却还记着）：报的是能读的配置错误，
    /// 指认文件并给出可检查的一步，而不是一条裸的 `os error 2`。
    #[test]
    fn reports_a_missing_host_file_as_a_readable_configuration_error() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");

        let error = inspect(&path).unwrap_err();
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
        let message = error.to_string();
        assert!(message.contains("extension.js"), "{message}");
        assert!(message.contains("readable"), "{message}");
    }

    /// 复核 12-devin/P3-2：还原指引必须挂在**真正会失败**的路径上。宿主文件被删掉后
    /// 恢复（以及状态接口的 inspect）走的是"读不到宿主文件"这一支，这里必须同时给出
    /// 备份路径与"复制回去"这一步；把指引拿掉，这两个断言就会红。
    #[test]
    fn a_restore_without_the_host_file_still_names_the_backup_and_the_copy_step() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        let receipt = apply(&path, ports()).unwrap();
        fs::remove_file(&path).unwrap();

        let backup = receipt.backup_path.display().to_string();
        for message in [
            restore(&receipt).unwrap_err().to_string(),
            inspect(&path).unwrap_err().to_string(),
        ] {
            assert!(message.contains(&backup), "提示里要有备份路径: {message}");
            assert!(
                message.contains("copy"),
                "提示里要给出复制回去这一步: {message}"
            );
            assert!(message.contains("backup:"), "界面按它取路径: {message}");
        }
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

    /// 替换在"宿主文件已离位、新内容还没落上"之间被打断（崩溃、强杀）时，宿主文件
    /// 不在原位，只剩离位副本。下一次读必须把它放回去，而不是报"文件读不出来"。
    #[test]
    fn a_displaced_copy_left_by_an_interrupted_replace_is_put_back() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();
        let patched = fs::read_to_string(&path).unwrap();
        // 模拟中断现场：宿主文件已被改名离位，新内容还没放上去。
        let displaced = displaced_path(&path);
        fs::rename(&path, &displaced).unwrap();
        assert!(!path.exists());

        let status = inspect(&path).unwrap();
        assert!(status.patched, "放回去的必须是补丁版本");
        assert_eq!(fs::read_to_string(&path).unwrap(), patched);
        assert!(!displaced.exists(), "离位副本不该留在原地");

        // 放回去之后收据重新可用，恢复入口照常工作。
        let receipt: PatchReceipt = serde_json::from_value(restore_receipt_of(&status)).unwrap();
        restore(&receipt).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), fixture());
    }

    /// 写入路径也会先把上次留下的离位副本放回去：否则 apply 只会报"宿主文件读不
    /// 出来"，用户看到的是一条无路可走的错误。
    #[test]
    fn apply_recovers_a_displaced_copy_before_writing() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();
        let displaced = displaced_path(&path);
        fs::rename(&path, &displaced).unwrap();

        // 补丁还在，只是躺在离位副本里：这次 apply 先把文件放回原位，再按"已经打过
        // 补丁"拒绝（fail-closed），而不是报文件读不出来。
        let error = apply(&path, ports()).unwrap_err().to_string();
        assert!(error.contains("already patched"), "{error}");
        assert!(path.is_file(), "宿主文件必须先被放回原位");
        assert!(!displaced.exists());
        assert!(inspect(&path).unwrap().patched);
    }

    /// 成功的替换不留离位副本，宿主文件就是补丁内容。
    #[test]
    fn a_successful_replace_leaves_no_displaced_copy_behind() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();
        assert!(!displaced_path(&path).exists());
        assert!(inspect(&path).unwrap().patched);
    }

    /// 强杀收尾：没有内存里的收据，靠落盘那份把宿主文件还原回干净版本。
    #[test]
    fn a_persisted_receipt_restores_the_host_file_without_any_in_memory_state() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        apply(&path, ports()).unwrap();

        assert!(restore_persisted(&path), "收据可用时必须真的恢复");
        assert_eq!(fs::read_to_string(&path).unwrap(), fixture());
        assert!(
            !restore_persisted(&path),
            "已经恢复过（收据已失效）之后不该再报成功"
        );
    }

    /// 没什么可恢复时收尾助手必须安静返回：宿主文件本来就没打过补丁，或者收据已经
    /// 被改动过（用户自己改过文件），都不该让收尾流程失败。
    #[test]
    fn a_persisted_restore_reports_nothing_to_do_instead_of_failing() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("extension.js");
        fs::write(&path, fixture()).unwrap();
        assert!(!restore_persisted(&path), "干净文件不该被当成可恢复");

        apply(&path, ports()).unwrap();
        fs::remove_file(receipt_file(&path)).unwrap();
        assert!(!restore_persisted(&path), "没有收据时不该报成功");
        assert!(inspect(&path).unwrap().patched, "宿主文件必须原样保留");

        assert!(
            !restore_persisted(&directory.path().join("missing.js")),
            "宿主文件不在了也不该让收尾流程失败"
        );
    }
}
