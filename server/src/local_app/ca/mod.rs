//! Installs and manages the local certificate authority.
use std::{fs, path::PathBuf};

#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, RsaKeySize, PKCS_RSA_SHA256,
};
use sha1::{Digest, Sha1};
use time::{Duration, OffsetDateTime};
use x509_parser::prelude::FromDer;

use crate::{config::managed_data_dir, Error, Result};

use super::CaState;

#[derive(Clone)]
pub struct CaManager {
    dir: PathBuf,
}

pub struct LoadedCa {
    pub issuer: Issuer<'static, KeyPair>,
}

impl CaManager {
    pub fn managed() -> Result<Self> {
        Ok(Self {
            dir: managed_data_dir()?.join("ca"),
        })
    }

    pub fn cert_path(&self) -> PathBuf {
        self.dir.join("ca.crt")
    }
    fn key_path(&self) -> PathBuf {
        self.dir.join("ca.key")
    }

    pub fn state(&self) -> Result<CaState> {
        let cert = fs::read_to_string(self.cert_path());
        let key = fs::read_to_string(self.key_path());
        match (cert, key) {
            (Err(cert_error), Err(key_error))
                if cert_error.kind() == std::io::ErrorKind::NotFound
                    && key_error.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(CaState::Missing)
            }
            (Ok(cert), Ok(key)) => {
                if parse_issuer(&cert, &key).is_err() {
                    return Ok(CaState::Invalid);
                }
                Ok(if is_installed(&cert)? {
                    CaState::Ready
                } else {
                    CaState::Untrusted
                })
            }
            _ => Ok(CaState::Invalid),
        }
    }

    pub fn load(&self) -> Result<LoadedCa> {
        let cert = fs::read_to_string(self.cert_path())?;
        let key = fs::read_to_string(self.key_path())?;
        Ok(LoadedCa {
            issuer: parse_issuer(&cert, &key)?,
        })
    }

    pub fn install_command(&self) -> Option<String> {
        let path = self.cert_path().to_string_lossy().replace('\'', "'\\''");
        match std::env::consts::OS {
            "macos" => dirs::home_dir().map(|_| {
                format!(
                    "sudo security add-trusted-cert -d -r trustRoot -p ssl -k /Library/Keychains/System.keychain '{}'",
                    path
                )
            }),
            "windows" => Some(format!(
                "certutil -addstore -f Root \"{}\"",
                self.cert_path().display()
            )),
            "linux" => {
                let anchor = linux_anchor_file();
                Some(format!(
                    "sudo cp '{}' '{}' && sudo {}",
                    path,
                    anchor.display(),
                    linux_refresh_command()
                ))
            }
            _ => None,
        }
    }

    pub fn initialize_local(&self) -> Result<()> {
        match self.state()? {
            // 半套或损坏的 CA 以前是死路：界面写着"证书文件无法使用，重新初始化会生成一份
            // 新的"，代码却每次都返回同一个错误。`generate()` 的两步写盘之间没有回滚，写
            // 完 key 之后失败（磁盘满、杀软占着 ca.crt、进程被杀）就正好落在这里——一次
            // 写失败足够让本地/混合模式永久打不开。现在按"缺文件"处理：先把这一份改名
            // 备份（里面有用户的私钥，不能直接扔），再生成一套新的。
            CaState::Invalid => {
                // 旧证书可能还在系统信任库里，而且仍然受信：`is_installed` 只按当前这份
                // 证书比对，重置不会顺手清掉那个根。先算出删它的命令，重置之后写进日志。
                let stale_root = stale_root_removal(self.cert_path()).ok();
                let backup = self.back_up_invalid()?;
                tracing::warn!(
                    backup = backup.as_ref().map(|path| path.display().to_string()),
                    stale_root = stale_root.as_deref().unwrap_or("unknown"),
                    "the local CA was incomplete or invalid; backed it up and generated a new one"
                );
                self.generate()?;
            }
            CaState::Ready => return Ok(()),
            CaState::Missing => self.generate()?,
            CaState::Untrusted => {}
        }
        Ok(())
    }

    /// 把不完整/损坏的 `ca/` 改名备份，返回备份目录。
    ///
    /// 备份而不是删除：私钥可能还能用（例如只是证书过期），"重置"这个动作不该顺手毁掉
    /// 用户的数据。备份留在数据目录里，日志会写出它的路径。
    fn back_up_invalid(&self) -> Result<Option<PathBuf>> {
        if !self.dir.exists() {
            return Ok(None);
        }
        let backup = self
            .dir
            .with_file_name(format!("ca-invalid-{}", crate::store::now_ms()));
        fs::rename(&self.dir, &backup)?;
        Ok(Some(backup))
    }

    fn generate(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;

        let key = KeyPair::generate_rsa_for(&PKCS_RSA_SHA256, RsaKeySize::_3072)
            .map_err(|error| Error::Config(format!("generate CA key: {error}")))?;
        let mut params = CertificateParams::new(Vec::<String>::new())
            .map_err(|error| Error::Config(format!("create CA parameters: {error}")))?;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, "haxsd byok Local CA");
        name.push(DnType::OrganizationName, "haxsd byok");
        params.distinguished_name = name;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        params.not_before = OffsetDateTime::now_utc() - Duration::minutes(5);
        params.not_after = OffsetDateTime::now_utc() + Duration::days(3652);
        let cert = params
            .self_signed(&key)
            .map_err(|error| Error::Config(format!("generate CA certificate: {error}")))?;
        write_atomic(&self.key_path(), key.serialize_pem().as_bytes(), 0o600)?;
        write_atomic(&self.cert_path(), cert.pem().as_bytes(), 0o644)?;
        Ok(())
    }
}

fn parse_issuer(cert: &str, key: &str) -> Result<Issuer<'static, KeyPair>> {
    let key =
        KeyPair::from_pem(key).map_err(|error| Error::Config(format!("parse CA key: {error}")))?;
    let pem = pem::parse(cert).map_err(|error| Error::Config(format!("parse CA PEM: {error}")))?;
    let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(pem.contents())
        .map_err(|error| Error::Config(format!("parse CA X.509 certificate: {error}")))?;
    if parsed.public_key().subject_public_key.data.as_ref() != key.public_key_raw() {
        return Err(Error::Config(
            "CA certificate and private key do not match".into(),
        ));
    }
    if !parsed.validity().is_valid() {
        return Err(Error::Config(
            "CA certificate is outside its validity period".into(),
        ));
    }
    if !parsed
        .basic_constraints()
        .map_err(|error| Error::Config(format!("read CA constraints: {error}")))?
        .is_some_and(|constraints| constraints.value.ca)
    {
        return Err(Error::Config("certificate is not a CA".into()));
    }
    Issuer::from_ca_cert_pem(cert, key)
        .map_err(|error| Error::Config(format!("parse CA certificate: {error}")))
}

fn write_atomic(path: &std::path::Path, data: &[u8], mode: u32) -> Result<()> {
    let temp = path.with_extension("tmp");
    write_private(&temp, data, mode)?;
    fs::rename(&temp, path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// 先定权限再写内容（unix）。
///
/// `fs::write` 以 `0o666 & ~umask`（通常 0644）创建文件，收紧权限发生在写完**之后**；
/// 而这里写的正是 CA 私钥。进程若恰在"内容已落盘、还没 chmod"之间被杀/断电，`ca.key`
/// 就以 0644 留在盘上——同机其他用户能读走它，进而伪造任意站点证书。`write_atomic`
/// 存在的意义正是防这种中断，所以权限要在文件**出现的那一刻**就是私有的。
/// 覆盖已存在的文件时 `O_CREAT` 的 mode 不生效，因此照旧再 `set_permissions` 一次。
fn write_private(path: &std::path::Path, data: &[u8], mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(path)?;
        file.write_all(data)?;
        drop(file);
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        fs::write(path, data)?;
        Ok(())
    }
}

/// 旧 CA 从系统信任库里删掉的命令。
///
/// 重置会生成一套新 CA，但旧的根还在系统信任库里、而且仍然受信（`is_installed` 按 DER
/// 只认当前这份证书，不会替我们清）。只把旧文件改名备份，用户的信任库里就多出一个永远
/// 用不到的根，所以这里把删它的命令算出来，跟备份路径一起写进日志。
fn stale_root_removal(cert_path: PathBuf) -> Result<String> {
    let cert = fs::read_to_string(cert_path)?;
    let fingerprint = fingerprint(&cert)?;
    Ok(match std::env::consts::OS {
        "macos" => format!(
            "sudo security delete-certificate -Z {fingerprint} /Library/Keychains/System.keychain"
        ),
        "windows" => format!("certutil -delstore Root {fingerprint}"),
        _ => linux_stale_root_removal(&fingerprint),
    })
}

/// Linux 的删除提示：文件式信任库只能按**路径**删，所以命令里没有证书名；指纹作为注释
/// 追加在后面（复制执行不受影响），用户在多份锚点/多台机器之间才能确认自己删掉的确实是
/// 受信的那一份。
///
/// 单独抽出来是因为平台分支在 CI 的 Linux runner 上才会走到：抽成纯函数之后，任何平台都能
/// 用用例钉住"Linux 提示必须带指纹"这条（旧实现把 `fingerprint` 算出来后在 `_` 分支里丢了，
/// macOS/Windows 都带、只有 Linux 不带，本机 Windows 跑全量永远发现不了）。
fn linux_stale_root_removal(fingerprint: &str) -> String {
    // 三个候选目录都可能留着一份（换过发行版、装过两次）：只删"当前那一个"，
    // 剩下的仍然受信。
    let mut commands = linux_installed_anchors()
        .into_iter()
        .map(|path| format!("sudo rm '{}'", path.display()))
        .collect::<Vec<_>>();
    if commands.is_empty() {
        commands.push(format!("sudo rm '{}'", linux_anchor_file().display()));
    }
    commands.push(format!("sudo {}", linux_refresh_command()));
    format!("{} # sha1 {fingerprint}", commands.join(" && "))
}

fn fingerprint(cert: &str) -> Result<String> {
    let pem = pem::parse(cert).map_err(|error| Error::Config(format!("parse CA PEM: {error}")))?;
    Ok(hex::encode_upper(Sha1::digest(pem.contents())))
}

/// 系统信任库里有没有**这一份**证书（按 DER 比对传入的 PEM，不看当前文件）。
///
/// 提权安装的回读确认必须用它，而不是重新读一遍 `ca.crt`：`certutil` / `security` 是在用户
/// 点过授权之后才去读那个用户可写文件的，若这期间文件被换掉，回读"当前文件在不在存储里"
/// 恰好会通过——而装进去的是换过的那一份，之后 `load()` 还会拿这对被换掉的证书与私钥去
/// 签发拦截证书。调用方要在提权**之前**把 PEM 读进内存，装完拿这一份来问。
pub fn is_certificate_installed(cert_pem: &str) -> Result<bool> {
    is_installed(cert_pem)
}

#[cfg(target_os = "macos")]
fn is_installed(cert: &str) -> Result<bool> {
    let fingerprint = fingerprint(cert)?;
    for keychain in ["login.keychain-db", "/Library/Keychains/System.keychain"] {
        let output = Command::new("security")
            .args(["find-certificate", "-a", "-Z", keychain])
            .output()?;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(&fingerprint)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "windows")]
fn is_installed(cert: &str) -> Result<bool> {
    windows::is_installed(cert)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn is_installed(cert: &str) -> Result<bool> {
    is_installed_in(cert, &LINUX_ANCHOR_DIRECTORIES)
}

/// 三个候选锚点目录里**任意一个**装了我们的证书就算已信任。
///
/// 以前只看"当前存在的那一个"目录：用户按提示在 `/usr/local/share/ca-certificates` 装好
/// 之后，某个包创建了 `/etc/pki/ca-trust/source/anchors`（或用户在另一台同源的机器上装过），
/// 检测就改去读另一个路径、报"未信任"，用户被反复要求重新装证书，而接管在 `Ready` 之前
/// 根本开不了。装过两次、换过发行版时会有多份，所以是"任一命中"。
///
/// 与平台无关（只读文件），所以在每个平台都编译并跑用例；真实调用点在 Linux 分支，
/// 因此非 Linux 上要显式允许"本平台没有调用者"。
#[cfg_attr(any(target_os = "macos", target_os = "windows"), allow(dead_code))]
fn is_installed_in(cert: &str, directories: &[&str]) -> Result<bool> {
    for path in linux_anchor_files_in(directories) {
        match fs::read_to_string(&path) {
            Ok(installed) if installed.trim() == cert.trim() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

const LINUX_ANCHOR_NAME: &str = "cursor-byok-local-ca.crt";

/// Linux 上各发行版的锚点目录，按检测顺序排列。
const LINUX_ANCHOR_DIRECTORIES: [&str; 3] = [
    "/etc/pki/ca-trust/source/anchors",
    "/etc/ca-certificates/trust-source/anchors",
    "/usr/local/share/ca-certificates",
];

fn linux_anchor_files_in(directories: &[&str]) -> Vec<PathBuf> {
    directories
        .iter()
        .map(|directory| PathBuf::from(directory).join(LINUX_ANCHOR_NAME))
        .collect()
}

/// 我们在这个目录里装了几份证书（三个候选都可能有一份）。
fn linux_installed_anchors() -> Vec<PathBuf> {
    linux_anchor_files_in(&LINUX_ANCHOR_DIRECTORIES)
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

/// 安装命令要写的那个锚点：选第一个**存在**的目录，都不存在就用 `update-ca-certificates`
/// 的标准位置（`/usr/local/share/ca-certificates`）。
fn linux_anchor_file() -> PathBuf {
    linux_anchor_files_in(&LINUX_ANCHOR_DIRECTORIES)
        .into_iter()
        .find(|path| path.parent().is_some_and(|directory| directory.is_dir()))
        .unwrap_or_else(|| PathBuf::from(LINUX_ANCHOR_DIRECTORIES[2]).join(LINUX_ANCHOR_NAME))
}

fn linux_refresh_command() -> &'static str {
    match linux_anchor_file().parent().and_then(|dir| dir.to_str()) {
        Some("/usr/local/share/ca-certificates") => "update-ca-certificates",
        _ => "update-ca-trust extract",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(directory: &tempfile::TempDir) -> CaManager {
        CaManager {
            dir: directory.path().join("ca"),
        }
    }

    fn backups(directory: &tempfile::TempDir) -> Vec<PathBuf> {
        fs::read_dir(directory.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("ca-invalid-"))
            })
            .collect()
    }

    /// 半套文件（`generate()` 写完 key 之后失败就是这个形状）以前是终态：`initialize_local`
    /// 每次都返回同一个错误，而界面写着"重新初始化会生成一份新的"。现在按"缺文件"重建，
    /// 并把坏掉的那一份改名备份——里面可能还有用户的私钥。
    #[test]
    fn an_invalid_ca_is_backed_up_and_regenerated() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager(&directory);
        fs::create_dir_all(&manager.dir).unwrap();
        fs::write(manager.key_path(), "-----BEGIN PRIVATE KEY-----\nbroken\n").unwrap();
        assert!(matches!(manager.state().unwrap(), CaState::Invalid));

        manager.initialize_local().unwrap();

        // 新的一套是配套的、在有效期内的 CA。
        manager.load().unwrap();
        // 坏掉的那一份没有消失，挪到了隔壁的备份目录里。
        let backups = backups(&directory);
        assert_eq!(backups.len(), 1, "坏掉的那一份必须留下一份备份");
        assert!(backups[0].join("ca.key").exists());
    }

    /// 一整套没坏的文件不能被"重置"动到：只有明确损坏时才重建。
    #[test]
    fn a_generated_ca_is_not_regenerated() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager(&directory);
        manager.initialize_local().unwrap();
        let cert = fs::read_to_string(manager.cert_path()).unwrap();

        manager.initialize_local().unwrap();

        assert_eq!(fs::read_to_string(manager.cert_path()).unwrap(), cert);
        assert!(backups(&directory).is_empty());
    }

    /// 删旧根的提示按平台给出可照做的命令，而且带上旧证书的指纹。
    #[test]
    fn the_stale_root_hint_carries_the_old_fingerprint() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager(&directory);
        manager.initialize_local().unwrap();
        let cert = fs::read_to_string(manager.cert_path()).unwrap();

        let hint = stale_root_removal(manager.cert_path()).unwrap();

        assert!(hint.contains(&fingerprint(&cert).unwrap()), "{hint}");
    }

    /// Linux 分支的提示必须带指纹。
    ///
    /// 平台分支只有在 Linux 上才走到，而"本机是 Windows/macOS"的开发者跑全量永远发现不了
    /// 这条（CI 的 Linux runner 上就是这么红的）：旧实现把 `fingerprint` 算出来之后，在
    /// `_` 分支里只拼了 rm + 刷新命令，指纹被丢掉。检查 `stale_root_removal` 的启发式在这里
    /// 用不了，所以直接钉住抽出来的纯函数。
    #[test]
    fn the_linux_stale_root_hint_carries_the_fingerprint() {
        let hint = linux_stale_root_removal("0123456789ABCDEF0123456789ABCDEF01234567");

        assert!(
            hint.contains("0123456789ABCDEF0123456789ABCDEF01234567"),
            "Linux 提示必须能识别出要删的是哪一份证书: {hint}"
        );
        assert!(hint.contains("sudo rm '"), "提示里要有删除命令: {hint}");
        assert!(
            hint.contains("update-ca-"),
            "提示里要有刷新信任库的命令: {hint}"
        );
    }

    /// CA 私钥从存在的那一刻起就必须是私有权限（unix）。
    ///
    /// 旧实现 `fs::write` 先以 0644 建文件、再 chmod：进程恰在两步之间被杀/断电，盘上就留下
    /// 一份同机他人可读的 CA 私钥。
    #[cfg(unix)]
    #[test]
    fn the_ca_key_is_private_from_the_moment_it_exists() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ca.tmp");
        write_private(&path, b"secret", 0o600).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // 覆盖已存在的宽松文件时同样要收紧（O_CREAT 的 mode 只在创建时生效）。
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"newer", 0o600).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // 走完 `write_atomic` 的 rename 之后也还是私有权限。
        let final_path = directory.path().join("ca.key");
        write_atomic(&final_path, b"secret", 0o600).unwrap();
        assert_eq!(
            fs::metadata(&final_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// 三个候选锚点里任意一个装了我们的证书就算已信任（Linux 的检测逻辑，与平台无关）。
    ///
    /// 旧实现只读"当前存在的那一个"目录：包管理器之后创建了另一个候选目录，状态就会从
    /// 已就绪退回"待系统信任"，用户被反复要求重装证书。
    ///
    /// 输入完全受控：候选目录列表是参数，用例只造自己的临时目录，不碰 `$HOME`、环境变量或
    /// 真实的 `/etc/...` 锚点——所以它与全量并行无关（最后两条断言就是在钉这个性质）。
    #[test]
    fn any_linux_anchor_candidate_counts_as_installed() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let absent = directory.path().join("absent");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let candidates = [
            first.to_str().unwrap(),
            second.to_str().unwrap(),
            absent.to_str().unwrap(),
        ];

        assert!(!is_installed_in("our cert", &candidates).unwrap());

        // 只有第二个候选装了（正是"安装位置与检测位置不一致"的那一幕）。
        fs::write(second.join(LINUX_ANCHOR_NAME), "our cert\n").unwrap();
        assert!(is_installed_in("our cert", &candidates).unwrap());
        assert!(!is_installed_in("another cert", &candidates).unwrap());

        // 判据只来自传进来的这份列表：空列表必须回答"未信任"，即检测不会去读列表之外的
        // 任何路径（真实的 `/etc/...`、`$HOME`、环境变量都不参与）。这条断言让"用例受环境
        // 影响"成为可证伪的性质，而不是靠约定。
        assert!(
            !is_installed_in("our cert", &[]).unwrap(),
            "空候选列表必须回答未信任：检测不能读候选列表之外的路径"
        );
        // 列表里只有不存在的目录时同理：不存在 ≠ 已信任。
        assert!(!is_installed_in("our cert", &[absent.to_str().unwrap()]).unwrap());
    }
}
