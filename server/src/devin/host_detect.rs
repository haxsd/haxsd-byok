//! Finds the Devin host extension without asking the user for a path.
//!
//! The patch boundary itself never discovers anything: it stays fail-closed and
//! requires an explicit path. Detection lives here so the UI works out of the box,
//! while an explicit path always wins.

use std::path::{Path, PathBuf};

/// Relative location of the Devin/Windsurf host extension inside an install.
const HOST_RELATIVE: &str = r"resources\app\extensions\windsurf\dist\extension.js";

/// An explicit file override wins over every guessed location.
const PATH_ENV: &str = "HAXSD_BYOK_DEVIN_PATH";

/// The install root can also be given directly, in which case only the relative
/// part is appended.
const ROOT_ENV: &str = "HAXSD_BYOK_DEVIN_ROOT";

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
            Self::NotFound { searched } => {
                let tried = searched
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                format!("未找到 Devin 安装，已尝试：{tried}")
            }
        }
    }
}

pub fn detect() -> Detected {
    detect_from_candidates(candidates())
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

    #[test]
    fn a_missing_installation_explains_everywhere_it_looked() {
        let detected = detect_from_candidates(vec![(
            PathBuf::from(r"C:\nowhere\extension.js"),
            DetectionSource::Guessed,
        )]);
        assert!(detected.path().is_none());
        let message = detected.explanation();
        assert!(message.contains("C:\\nowhere\\extension.js"), "{message}");
    }

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
}
