//! Finds the Devin host extension without asking the user for a path.
//!
//! The patch boundary itself never discovers anything: it stays fail-closed and
//! requires an explicit path. Detection lives here so the UI works out of the box,
//! while an explicit path always wins.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

/// Relative location of the Devin/Windsurf host extension inside an install.
pub(crate) const HOST_RELATIVE: &str = r"resources\app\extensions\windsurf\dist\extension.js";

/// An explicit file override wins over every guessed location.
const PATH_ENV: &str = "HAXSD_BYOK_DEVIN_PATH";

/// The install root can also be given directly, in which case only the relative
/// part is appended.
const ROOT_ENV: &str = "HAXSD_BYOK_DEVIN_ROOT";

/// Bounded search for an installed client. The install directory itself cannot be
/// enumerated — the same client was found under `F:\app\windsurf-app\Windsurf` —
/// while the executable name is fixed, so the scan looks for the executable
/// instead of guessing directory names. Depth and a directory budget keep it
/// cheap, and the shallowest directories are visited first.
const SCAN_MAX_DEPTH: usize = 4;
const SCAN_DIRECTORY_BUDGET: usize = 3_000;

/// Never hold a client install, so descending only spends the budget. Per-user
/// installs live under `users`/`programdata`, but the environment-derived
/// candidates already cover those without a scan.
const SCAN_SKIP: [&str; 10] = [
    "windows",
    "$recycle.bin",
    "system volume information",
    "recovery",
    "perflogs",
    "users",
    "programdata",
    "node_modules",
    ".git",
    ".svn",
];

/// Where a found path came from, so the UI can tell a user choice from a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectionSource {
    ExplicitPath,
    Guessed,
}

impl DetectionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitPath => "explicit",
            Self::Guessed => "detected",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Detected {
    Found {
        path: PathBuf,
        source: DetectionSource,
    },
    /// Nothing matched; the searched candidates let the failure explain itself
    /// instead of surfacing as "unknown".
    NotFound { searched: Vec<PathBuf> },
}

impl Detected {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Found { path, .. } => Some(path),
            Self::NotFound { .. } => None,
        }
    }

    pub fn source(&self) -> Option<DetectionSource> {
        match self {
            Self::Found { source, .. } => Some(*source),
            Self::NotFound { .. } => None,
        }
    }

    pub fn explanation(&self) -> String {
        match self {
            Self::Found { .. } => String::new(),
            // 与其它服务端错误一样用英文：这些字符串会直接进界面提示，而界面
            // 语言是可切换的，服务端不该替用户选一种。
            //
            // 只报搜索规模，不铺候选路径：几十条猜过的路径挤在提示框里，用户
            // 拿不到任何下一步动作。真正的出路是"填宿主文件路径"，所以直接说它。
            Self::NotFound { searched } => format!(
                "Devin installation not found; {} locations were searched. \
                 Fill in the host file path below: it is the file inside the installation at \
                 <install directory>\\{HOST_RELATIVE}",
                searched.len()
            ),
        }
    }
}

pub fn detect() -> Detected {
    // The fixed candidates are free and almost always hit; the bounded scan is a
    // second pass so that paying for it stays the exception.
    match detect_from_candidates(candidates()) {
        found @ Detected::Found { .. } => found,
        Detected::NotFound { mut searched } => {
            let scanned = detect_from_candidates(
                scanned_installs()
                    .into_iter()
                    .map(|install| (install.join(HOST_RELATIVE), DetectionSource::Guessed))
                    .collect(),
            );
            match scanned {
                found @ Detected::Found { .. } => found,
                Detected::NotFound {
                    searched: scanned_searched,
                } => {
                    searched.extend(scanned_searched);
                    Detected::NotFound { searched }
                }
            }
        }
    }
}

/// Installation directories found by scanning the fixed drives for the client
/// executables.
fn scanned_installs() -> Vec<PathBuf> {
    let roots: Vec<PathBuf> = ['C', 'D', 'E', 'F', 'G']
        .into_iter()
        .map(|letter| PathBuf::from(format!("{letter}:\\")))
        .collect();
    scan_for_installs(&roots)
}

/// Split out so a test can scan a temporary tree instead of real drives.
fn scan_for_installs(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut installs: Vec<PathBuf> = Vec::new();
    let mut pending: VecDeque<(PathBuf, usize)> =
        roots.iter().cloned().map(|root| (root, 0)).collect();
    let mut visited = 0_usize;
    while let Some((directory, depth)) = pending.pop_front() {
        if visited >= SCAN_DIRECTORY_BUDGET {
            break;
        }
        visited += 1;
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if kind.is_file() {
                let is_client = APP_EXECUTABLES
                    .iter()
                    .any(|executable| executable.eq_ignore_ascii_case(name));
                if is_client && !installs.contains(&directory) {
                    installs.push(directory.clone());
                }
                continue;
            }
            if !kind.is_dir() || depth >= SCAN_MAX_DEPTH {
                continue;
            }
            if SCAN_SKIP.iter().any(|skip| skip.eq_ignore_ascii_case(name)) {
                continue;
            }
            // Product-named directories are descended first: an early hit keeps the
            // budget for the remaining drives.
            let named_after_product = name.to_ascii_lowercase();
            let named_after_product =
                named_after_product.contains("devin") || named_after_product.contains("windsurf");
            if named_after_product {
                pending.push_front((entry.path(), depth + 1));
            } else {
                pending.push_back((entry.path(), depth + 1));
            }
        }
    }
    installs
}

/// Split out so the search order can be tested without touching the real machine.
fn detect_from_candidates(candidates: Vec<(PathBuf, DetectionSource)>) -> Detected {
    let mut searched = Vec::new();
    for (candidate, source) in candidates {
        if candidate.is_file() {
            return Detected::Found {
                path: candidate,
                source,
            };
        }
        searched.push(candidate);
    }
    Detected::NotFound { searched }
}

fn candidates() -> Vec<(PathBuf, DetectionSource)> {
    let mut candidates = Vec::new();

    if let Some(explicit) = non_empty_env(PATH_ENV) {
        candidates.push((PathBuf::from(explicit), DetectionSource::ExplicitPath));
    }
    for root in roots() {
        candidates.push((root.join(HOST_RELATIVE), DetectionSource::Guessed));
    }
    // The executable name is the reliable signal: installs nest the app at
    // unknown depth, so committing to a fixed layout guesses wrong. Locating
    // `<product>.exe` and deriving the extension from its directory does not.
    for root in scan_roots() {
        for executable in APP_EXECUTABLES {
            let app = root.join(executable);
            if !app.is_file() {
                continue;
            }
            if let Some(directory) = app.parent() {
                candidates.push((directory.join(HOST_RELATIVE), DetectionSource::Guessed));
            }
        }
    }
    candidates
}

/// Executable names that identify a Devin or Windsurf install root.
const APP_EXECUTABLES: [&str; 2] = ["Devin.exe", "Windsurf.exe"];

/// Directories that can contain the installation, one or two levels deep. This is
/// a bounded scan, never a recursive walk of a whole drive.
fn scan_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut first_level: Vec<PathBuf> = Vec::new();
    for letter in ['C', 'D', 'E', 'F', 'G'] {
        for name in ["devin", "Devin", "windsurf", "Windsurf"] {
            first_level.push(PathBuf::from(format!("{letter}:\\{name}")));
            first_level.push(PathBuf::from(format!("{letter}:\\Program Files\\{name}")));
        }
    }
    for base in [
        "LOCALAPPDATA",
        "APPDATA",
        "ProgramFiles",
        "ProgramFiles(x86)",
    ] {
        let Some(directory) = non_empty_env(base) else {
            continue;
        };
        for name in ["Devin", "Windsurf", "Programs\\Devin", "Programs\\Windsurf"] {
            first_level.push(PathBuf::from(&directory).join(name));
        }
    }
    roots.extend(first_level.iter().cloned());
    // One level deeper covers layouts such as D:\devin\Devin\Devin.exe.
    for directory in &first_level {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                roots.push(path);
            }
        }
    }
    roots
}

fn roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(root) = non_empty_env(ROOT_ENV) {
        roots.push(PathBuf::from(root));
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        let Some(base) = non_empty_env(variable) else {
            continue;
        };
        for product in ["Devin", "Windsurf"] {
            roots.push(PathBuf::from(&base).join(product));
        }
    }
    // Tools are often installed off the system drive; walking the fixed drive
    // letters keeps that working without a registry dependency.
    for letter in ['C', 'D', 'E', 'F', 'G'] {
        for name in ["devin", "Devin", "windsurf", "Windsurf"] {
            roots.push(PathBuf::from(format!("{letter}:\\{name}")));
            roots.push(PathBuf::from(format!("{letter}:\\Program Files\\{name}")));
        }
    }
    roots
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn existing_file() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")
    }

    #[test]
    fn the_first_existing_candidate_wins_and_keeps_its_source() {
        let detected = detect_from_candidates(vec![
            (
                PathBuf::from(r"D:\missing\extension.js"),
                DetectionSource::Guessed,
            ),
            (existing_file(), DetectionSource::ExplicitPath),
        ]);
        assert_eq!(detected.source(), Some(DetectionSource::ExplicitPath));
        assert_eq!(detected.path(), Some(existing_file().as_path()));
    }

    /// 真实布局里有这种安装：客户端被放进用户自建的容器目录，名字与产品无关
    /// （`F:\app\windsurf-app\Windsurf`），固定候选永远猜不到，只有按可执行文件
    /// 名扫描才发现得了。
    #[test]
    fn the_scan_finds_an_install_inside_a_user_named_container() {
        let directory = tempfile::tempdir().unwrap();
        let install = directory
            .path()
            .join("app")
            .join("windsurf-app")
            .join("Windsurf");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(install.join("Windsurf.exe"), b"").unwrap();

        let other = directory.path().join("app").join("cursor-app");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("Cursor.exe"), b"").unwrap();

        assert_eq!(
            scan_for_installs(&[directory.path().to_path_buf()]),
            vec![install]
        );
    }

    /// 预算只买来这么多目录：跳过系统目录、并且不越过深度上限，扫描才敢在真实
    /// 盘符上跑。
    #[test]
    fn the_scan_stays_inside_its_depth_and_skip_rules() {
        let directory = tempfile::tempdir().unwrap();
        let skipped = directory.path().join("Windows");
        std::fs::create_dir_all(&skipped).unwrap();
        std::fs::write(skipped.join("Windsurf.exe"), b"").unwrap();

        let too_deep = directory.path().join("a/b/c/d/e");
        std::fs::create_dir_all(&too_deep).unwrap();
        std::fs::write(too_deep.join("Devin.exe"), b"").unwrap();

        assert!(scan_for_installs(&[directory.path().to_path_buf()]).is_empty());
    }

    #[test]
    fn a_missing_installation_explains_what_to_do_instead_of_every_candidate() {
        let detected = detect_from_candidates(vec![
            (
                PathBuf::from(r"C:\nowhere\extension.js"),
                DetectionSource::Guessed,
            ),
            (
                PathBuf::from(r"D:\nowhere\extension.js"),
                DetectionSource::Guessed,
            ),
        ]);
        assert!(detected.path().is_none());
        let message = detected.explanation();
        assert!(message.contains("2 locations"), "{message}");
        assert!(message.contains(HOST_RELATIVE), "{message}");
        assert!(!message.contains(r"C:\nowhere"), "{message}");
    }

    /// The drive-letter layout only exists on Windows, so the assertion is
    /// platform-gated; a Linux runner has no ProgramFiles to expand.
    #[cfg(windows)]
    #[test]
    fn guessed_roots_cover_a_secondary_drive_install() {
        let roots = roots();
        assert!(
            roots.contains(&PathBuf::from(r"D:\devin")),
            "a D: drive install must be searched"
        );
        assert!(
            roots.iter().any(|root| root.ends_with("Devin")),
            "the Program Files layout must be searched"
        );
    }

    /// Outside Windows the scan still has to produce the portable candidates,
    /// derived from the environment rather than from drive letters.
    #[cfg(not(windows))]
    #[test]
    fn guessed_roots_stay_portable_off_windows() {
        let roots = roots();
        assert!(!roots.is_empty(), "some candidates must always exist");
        assert!(
            roots
                .iter()
                .all(|root| root.is_absolute() || root.is_relative()),
            "candidates must be usable paths"
        );
    }
}
