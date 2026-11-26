//! Configures process resource limits before the desktop runtime starts.

#[cfg(unix)]
use std::io;

#[cfg(unix)]
pub(crate) const REQUESTED_OPEN_FILE_LIMIT: u64 = 65_536;

/// macOS exposes `OPEN_MAX` in `sys/syslimits.h`, not through the `libc` crate.
/// Beyond this value `setrlimit` returns `EINVAL` on macOS.
#[cfg(target_os = "macos")]
const MACOS_OPEN_MAX: libc::rlim_t = 10_240;

#[cfg(unix)]
pub(crate) struct OpenFileLimit {
    pub(crate) previous: u64,
    pub(crate) effective: u64,
    pub(crate) hard: u64,
}

#[cfg(unix)]
pub(crate) fn raise_open_file_limit() -> io::Result<OpenFileLimit> {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limits` points to writable memory for one `rlimit` value.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let previous = limits.rlim_cur;
    #[cfg(target_os = "macos")]
    let max_requested = REQUESTED_OPEN_FILE_LIMIT.min(MACOS_OPEN_MAX as u64);
    #[cfg(not(target_os = "macos"))]
    let max_requested = REQUESTED_OPEN_FILE_LIMIT;
    let target = limits.rlim_max.min(max_requested as libc::rlim_t);
    if previous < target {
        let requested = libc::rlimit {
            rlim_cur: target,
            rlim_max: limits.rlim_max,
        };
        // SAFETY: `requested` is a valid `rlimit` value and does not raise the hard limit.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &requested) } != 0 {
            let error = io::Error::last_os_error();
            let fallback_target = limits.rlim_max.min(10_240 as libc::rlim_t);
            if error.raw_os_error() == Some(libc::EINVAL)
                && previous < fallback_target
                && fallback_target < target
            {
                let fallback = libc::rlimit {
                    rlim_cur: fallback_target,
                    rlim_max: limits.rlim_max,
                };
                if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &fallback) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            } else {
                return Err(error);
            }
        }
    }

    let mut effective = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `effective` points to writable memory for one `rlimit` value.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut effective) } != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(OpenFileLimit {
        previous,
        effective: effective.rlim_cur,
        hard: effective.rlim_max,
    })
}
