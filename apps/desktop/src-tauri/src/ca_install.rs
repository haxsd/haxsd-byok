//! Opens the operating-system authorization flow for this application's CA only.

pub(super) fn open_install_terminal() -> Result<(), String> {
    let ca = cursor_server::local_app::CaManager::managed().map_err(|error| error.to_string())?;
    match ca.state().map_err(|error| error.to_string())? {
        cursor_server::local_app::CaState::Ready => return Ok(()),
        cursor_server::local_app::CaState::Untrusted => {}
        _ => return Err("Initialize the local CA before installing its certificate".into()),
    }
    #[cfg(windows)]
    {
        let system_directory = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cannot locate the Windows system directory".to_string())?
            .join("System32");
        let certificate_path = ca.cert_path();
        let install_path = certificate_path.clone();
        install_and_confirm(
            // 授权前读进内存：certutil 是在用户点过授权之后才去读这个用户可写文件的。
            move || {
                std::fs::read_to_string(&certificate_path)
                    .map_err(|error| format!("Cannot read the CA certificate: {error}"))
            },
            || {
                let request = windows_install_request(&system_directory, &install_path)?;
                install_windows(request)
            },
            |certificate| {
                cursor_server::local_app::is_certificate_installed(certificate)
                    .map_err(|error| error.to_string())
            },
        )
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .args(["-a", "Terminal"])
            .status()
            .map_err(|error| error.to_string())?
            .success()
            .then_some(())
            .ok_or_else(|| "Could not open Terminal for CA installation".into())
    }
    #[cfg(target_os = "linux")]
    {
        const TERMINALS: &[(&str, &[&str])] = &[
            ("x-terminal-emulator", &["-e"]),
            ("gnome-terminal", &["--"]),
            ("konsole", &["-e"]),
            ("xfce4-terminal", &["--execute"]),
            ("alacritty", &["-e"]),
            ("kitty", &[]),
        ];
        let command = ca
            .install_command()
            .ok_or_else(|| "CA installation is unavailable".to_string())?;
        let script = format!("{command}; exec bash");
        for (terminal, separator) in TERMINALS {
            match std::process::Command::new(terminal)
                .args(*separator)
                .args(["bash", "-c", &script])
                .spawn()
            {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("No supported terminal emulator found".into())
    }
}

/// 提权安装 + 回读确认：确认的对象必须是**授权前读到的那一份**证书。
///
/// `certutil` 是在用户点过 UAC 之后才去读 `ca.crt` 的，而那个文件在用户可写目录里。若这期间
/// 文件被连证书带私钥一起换掉，`certutil` 装进去的就是换过的那一份；此时"重新读一遍文件再
/// 看它在不在存储里"（旧实现走 `CaState::Ready`）恰好会通过——随后 `load()` 还会用这对被换掉
/// 的证书与私钥去签发拦截证书。所以先把 PEM 读进内存，装完拿内存里这一份问系统存储。
///
/// 三段都参数化：用例要能分别注入"读到的证书""安装是否成功""存储里有没有这份证书"，
/// 覆盖"文件在授权期间被换掉"这个分支。
#[cfg(windows)]
fn install_and_confirm(
    read_certificate: impl FnOnce() -> Result<String, String>,
    install: impl FnOnce() -> Result<(), String>,
    is_installed: impl FnOnce(&str) -> Result<bool, String>,
) -> Result<(), String> {
    let certificate = read_certificate()?;
    install()?;
    if is_installed(&certificate)? {
        return Ok(());
    }
    Err(
        "Certificate installation finished, but the certificate we asked for is not in the system \
         trust store; it may have been replaced while the authorization prompt was open"
            .into(),
    )
}

#[cfg(windows)]
fn install_windows(request: WindowsInstallRequest) -> Result<(), String> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_CANCELLED, WAIT_OBJECT_0},
        System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE},
        UI::{
            Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW},
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    };

    let verb = request
        .verb
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let file = request
        .file
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let parameters = request
        .parameters
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // The owned UTF-16 buffers outlive ShellExecuteExW; no shell interprets the certificate path.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.lpDirectory = ptr::null();
    info.nShow = SW_SHOWNORMAL;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let code = unsafe { GetLastError() };
        return Err(if code == ERROR_CANCELLED {
            "Administrator authorization was cancelled".into()
        } else {
            format!(
                "Could not start CA installation: {}",
                std::io::Error::from_raw_os_error(code as i32)
            )
        });
    }
    if info.hProcess.is_null() {
        return Err("CA installer did not provide a process handle".into());
    }
    let result = (|| {
        if unsafe { WaitForSingleObject(info.hProcess, INFINITE) } != WAIT_OBJECT_0 {
            return Err(format!(
                "Could not wait for CA installation: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut exit_code = 0;
        if unsafe { GetExitCodeProcess(info.hProcess, &mut exit_code) } == 0 {
            return Err(format!(
                "Could not read CA installation result: {}",
                std::io::Error::last_os_error()
            ));
        }
        if exit_code != 0 {
            return Err(format!("CA installation failed with exit code {exit_code}"));
        }
        Ok(())
    })();
    // SEE_MASK_NOCLOSEPROCESS transfers ownership of this process handle to us.
    unsafe { CloseHandle(info.hProcess) };
    result
}

#[cfg(windows)]
struct WindowsInstallRequest {
    verb: &'static str,
    file: std::path::PathBuf,
    parameters: String,
}

#[cfg(windows)]
fn windows_install_request(
    system_directory: &std::path::Path,
    certificate: &std::path::Path,
) -> Result<WindowsInstallRequest, String> {
    let certificate_text = certificate
        .to_str()
        .ok_or_else(|| "CA certificate path is not valid Unicode".to_string())?;
    if !system_directory.is_absolute()
        || !certificate.is_absolute()
        || certificate_text.contains(['"', '\0'])
    {
        return Err("CA installation requires an absolute, safely quoted certificate path".into());
    }
    Ok(WindowsInstallRequest {
        verb: "runas",
        file: system_directory.join("certutil.exe"),
        parameters: format!("-addstore -f Root \"{certificate_text}\""),
    })
}

#[cfg(all(test, windows))]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn windows_ca_installation_requests_elevation_without_shell_interpolation() {
        let request = windows_install_request(
            Path::new(r"C:\Windows\System32"),
            Path::new(r"D:\My files\O'Neil & 数据\ca.crt"),
        )
        .unwrap();
        assert_eq!(request.verb, "runas");
        assert_eq!(request.file, Path::new(r"C:\Windows\System32\certutil.exe"));
        assert_eq!(
            request.parameters,
            "-addstore -f Root \"D:\\My files\\O'Neil & 数据\\ca.crt\""
        );
    }

    #[test]
    fn windows_ca_installation_rejects_relative_or_injected_paths() {
        let system = Path::new(r"C:\Windows\System32");
        assert!(windows_install_request(system, Path::new("ca.crt")).is_err());
        assert!(
            windows_install_request(system, Path::new("D:\\ca\" -delstore Root\\ca.crt")).is_err()
        );
        assert!(windows_install_request(Path::new("System32"), Path::new(r"D:\ca.crt")).is_err());
    }

    /// 回读确认问的必须是"我们请求装的那一份"，不是"当前文件里那一份"。
    ///
    /// 旧实现装完重新读 `ca.crt` 再走 `CaState::Ready`：授权窗口期间文件被换掉时，
    /// `certutil` 装的是换过的那份，而回读恰好通过。用例让"读证书"这个动作第二次返回
    /// **换过的那份**（存储里也只有那份），于是"装完再读一次"的写法会走进通过分支。
    #[test]
    fn the_installation_confirmation_checks_the_certificate_we_asked_for() {
        let asked = "the certificate we asked for";
        let swapped = "the certificate swapped in while the prompt was open";

        // 文件在授权期间被换掉：安装一动，之后读到的就是换过的那份；存储里也只有那份。
        let installed = std::cell::Cell::new(false);
        let read_before_install = std::cell::Cell::new(false);
        let swapped_case = install_and_confirm(
            || {
                read_before_install.set(!installed.get());
                Ok(String::from(if installed.get() { swapped } else { asked }))
            },
            || {
                installed.set(true);
                Ok(())
            },
            |certificate| Ok(certificate == swapped),
        );
        assert!(
            read_before_install.get(),
            "证书必须在提权之前读一次就定下来（装完再读会读到换过的那份）"
        );
        assert!(
            swapped_case
                .unwrap_err()
                .contains("not in the system trust store"),
            "装进去的不是我们请求的那一份时必须报出来"
        );

        // 请求的那份在存储里 → 通过。
        assert!(install_and_confirm(
            || Ok(asked.into()),
            || Ok(()),
            |certificate| Ok(certificate == asked),
        )
        .is_ok());

        // 读不到证书就不要提权。
        assert_eq!(
            install_and_confirm(
                || Err("Cannot read the CA certificate".into()),
                || panic!("must not install without a certificate"),
                |_| panic!("must not check without a certificate"),
            )
            .unwrap_err(),
            "Cannot read the CA certificate"
        );

        // 授权被取消 / 安装失败：不再回读。
        assert_eq!(
            install_and_confirm(
                || Ok("certificate".into()),
                || Err("Administrator authorization was cancelled".into()),
                |_| panic!("must not check after a failed install"),
            )
            .unwrap_err(),
            "Administrator authorization was cancelled"
        );
    }
}
