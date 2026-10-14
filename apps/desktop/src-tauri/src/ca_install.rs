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
        let request = windows_install_request(&system_directory, &ca.cert_path())?;
        install_windows(request)?;
        // 证书文件在用户可写目录里，而提权后的 certutil 是在授权之后才去读它：
        // 装完回读存储确认在我们名下，避免"装上了，但不是这份"。
        match ca.state().map_err(|error| error.to_string())? {
            cursor_server::local_app::CaState::Ready => Ok(()),
            _ => Err(
                "Certificate installation finished, but the certificate is not in the system \
                 trust store; it may have been replaced while the authorization prompt was open"
                    .into(),
            ),
        }
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
}
