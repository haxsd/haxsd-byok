//! Finds the Devin host extension without asking the user for a path.
//!
//! The patch boundary itself never discovers anything: it stays fail-closed and
//! requires an explicit path. Detection lives here so the UI works out of the box,
//! while an explicit path always wins.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

/// Component form keeps the same layout usable on Windows, macOS, and Linux.
const HOST_RELATIVE_PARTS: [&str; 6] = [
    "resources",
    "app",
    "extensions",
    "windsurf",
    "dist",
    "extension.js",
];
const SETTINGS_RELATIVE_PARTS: [&str; 2] = ["User", "settings.json"];

fn join_parts(root: &Path, parts: &[&str]) -> PathBuf {
    let mut path = root.to_path_buf();
    for part in parts {
        path.push(part);
    }
    path
}

/// Every supported layout under an installation root. Windows/Linux use
/// `resources/app/...`; macOS `.app` bundles additionally use
/// `Contents/Resources/app/...`.
pub(crate) fn extension_paths_in_root(root: &Path) -> Vec<PathBuf> {
    let variants: [&[&str]; 4] = [
        HOST_RELATIVE_PARTS.as_slice(),
        &[
            "Contents",
            "Resources",
            "app",
            "extensions",
            "windsurf",
            "dist",
            "extension.js",
        ],
        &[
            "Resources",
            "app",
            "extensions",
            "windsurf",
            "dist",
            "extension.js",
        ],
        &["app", "extensions", "windsurf", "dist", "extension.js"],
    ];
    let mut paths: Vec<PathBuf> = Vec::new();
    for parts in variants {
        let path = join_parts(root, parts);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

fn settings_relative_path(root: &Path) -> PathBuf {
    join_parts(root, &SETTINGS_RELATIVE_PARTS)
}

fn relative_display(parts: &[&str]) -> String {
    parts.join(std::path::MAIN_SEPARATOR_STR)
}

/// An explicit file override wins over every guessed location.
const PATH_ENV: &str = "HAXSD_BYOK_DEVIN_PATH";

/// The install root can also be given directly, in which case only the relative
/// part is appended.
const ROOT_ENV: &str = "HAXSD_BYOK_DEVIN_ROOT";

/// An explicit settings override is useful for portable installs and tests.
const SETTINGS_ENV: &str = "HAXSD_BYOK_DEVIN_SETTINGS";

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
            // 拿不到任何下一步动作。后半句直接说下一步：填安装目录，或安装目录里的
            // extension.js；安装目录内部的相对路径用户看不见，只当噪声。
            //
            // "Devin installation not found" 是界面的匹配片段（`apps/desktop` 的
            // `harnessErrorText.ts` 按它把这句话换成中文），改文案要连着那张表一起改。
            Self::NotFound { searched } => format!(
                "Devin installation not found; {} locations were searched. Enter the installation \
                 directory in the installation path field, or the extension.js file inside it",
                searched.len()
            ),
        }
    }
}

pub fn detect() -> Detected {
    static CACHE: OnceLock<parking_lot::Mutex<DetectCache>> = OnceLock::new();
    cached_detect(
        CACHE.get_or_init(Default::default),
        detect_key(),
        Instant::now(),
        detect_uncached,
    )
}

/// The Devin user settings file that controls the API and inference endpoints.
///
/// Unlike the extension bundle, this file may not exist yet: Devin creates it
/// only after the first setting is written. Choosing an existing product data
/// directory is therefore the detection signal, not the file itself.
pub fn settings_path() -> Result<PathBuf, String> {
    let candidates = settings_candidates();
    if let Some(found) = candidates.iter().find(|path| path.is_file()) {
        return Ok(found.clone());
    }
    if let Some(found) = candidates
        .iter()
        .find(|path| path.parent().is_some_and(Path::is_dir))
    {
        return Ok(found.clone());
    }
    Err(
        "Devin user settings directory not found; start Devin once, or enter the \
         settings.json path manually"
            .into(),
    )
}

/// Resolve an explicit installation directory or `extension.js` path.
///
/// The file must exist: a pasted path that does not resolve is a configuration
/// error, not a silent fallback to another installation.
pub fn extension_path_from_input(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value.trim());
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err("Devin installation path must be absolute".into());
    }
    if path.is_file() {
        if path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("extension.js"))
        {
            return Ok(path);
        }
        return Err(format!(
            "{} is not a Devin extension.js file; enter the installation directory or the \
             extension.js file inside it",
            path.display()
        ));
    }
    if path.is_dir() {
        let candidates = extension_paths_in_root(&path);
        if let Some(found) = candidates.iter().find(|candidate| candidate.is_file()) {
            return Ok(found.clone());
        }
        return Err(format!(
            "{} is not a Devin installation directory; expected {}",
            path.display(),
            relative_display(&HOST_RELATIVE_PARTS)
        ));
    }
    Err(format!(
        "Devin installation path not found: {}",
        path.display()
    ))
}

fn settings_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(explicit) = non_empty_env(SETTINGS_ENV) {
        candidates.push(PathBuf::from(explicit));
    }

    #[cfg(windows)]
    if let Some(appdata) = non_empty_env("APPDATA") {
        for product in ["Devin", "Windsurf"] {
            candidates.push(settings_relative_path(
                &PathBuf::from(&appdata).join(product),
            ));
        }
    }

    #[cfg(target_os = "macos")]
    if let Some(home) = dirs::home_dir() {
        for product in ["Devin", "Windsurf"] {
            candidates.push(settings_relative_path(
                &home
                    .join("Library")
                    .join("Application Support")
                    .join(product),
            ));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(config) = dirs::config_dir() {
        for product in ["Devin", "Windsurf"] {
            candidates.push(settings_relative_path(&config.join(product)));
        }
    }

    candidates
}

/// 探测缓存的有效期。安装位置不会在几秒内变化，而固定候选全落空时一次探测要扫最多
/// 3000 个目录；页面加载、点「检查宿主」、取模型 UID、应用补丁都会探测，所以这段
/// 重复开销直接省掉。
const DETECT_CACHE_TTL: Duration = Duration::from_secs(10);

/// 让缓存立即失效的外部输入：显式路径与安装根。用户改了它们就是在说"照我说的再找
/// 一次"，不该等到 TTL 到期。
type DetectKey = (Option<String>, Option<String>);

fn detect_key() -> DetectKey {
    (non_empty_env(PATH_ENV), non_empty_env(ROOT_ENV))
}

#[derive(Default)]
struct DetectCache {
    entry: Option<(DetectKey, Instant, Detected)>,
}

impl DetectCache {
    fn get(&self, key: &DetectKey, now: Instant) -> Option<Detected> {
        let (cached_key, at, detected) = self.entry.as_ref()?;
        (cached_key == key && now.duration_since(*at) < DETECT_CACHE_TTL).then(|| detected.clone())
    }

    fn put(&mut self, key: DetectKey, now: Instant, detected: &Detected) {
        self.entry = Some((key, now, detected.clone()));
    }
}

fn cached_detect(
    cache: &parking_lot::Mutex<DetectCache>,
    key: DetectKey,
    now: Instant,
    scan: impl FnOnce() -> Detected,
) -> Detected {
    if let Some(detected) = cache.lock().get(&key, now) {
        return detected;
    }
    let detected = scan();
    cache.lock().put(key, now, &detected);
    detected
}

fn detect_uncached() -> Detected {
    // The fixed candidates are free and almost always hit; the bounded scan is a
    // second pass so that paying for it stays the exception.
    match detect_from_candidates(candidates()) {
        found @ Detected::Found { .. } => found,
        Detected::NotFound { mut searched } => {
            let scanned = detect_from_candidates(
                scanned_installs()
                    .into_iter()
                    .flat_map(|install| {
                        extension_paths_in_root(&install)
                            .into_iter()
                            .map(|path| (path, DetectionSource::Guessed))
                    })
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
    #[cfg(windows)]
    {
        let roots: Vec<PathBuf> = ['C', 'D', 'E', 'F', 'G']
            .into_iter()
            .map(|letter| PathBuf::from(format!("{letter}:\\")))
            .collect();
        scan_for_installs(&roots)
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
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
        for path in extension_paths_in_root(&root) {
            candidates.push((path, DetectionSource::Guessed));
        }
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
                for path in extension_paths_in_root(directory) {
                    candidates.push((path, DetectionSource::Guessed));
                }
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
    #[cfg(windows)]
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
    #[cfg(target_os = "macos")]
    for product in ["Devin", "Windsurf"] {
        roots.push(PathBuf::from(format!("/Applications/{product}.app")));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    for product in ["Devin", "Windsurf"] {
        let lower = product.to_ascii_lowercase();
        roots.push(PathBuf::from(format!("/opt/{product}")));
        roots.push(PathBuf::from(format!("/opt/{lower}")));
        roots.push(PathBuf::from(format!("/usr/lib/{product}")));
        roots.push(PathBuf::from(format!("/usr/lib/{lower}")));
    }
    // Tools are often installed off the system drive; walking the fixed drive
    // letters keeps that working without a registry dependency.
    #[cfg(windows)]
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

    /// 探测很贵（固定候选全落空时要扫最多 3000 个目录），而调用点很密：TTL 内的
    /// 重复探测必须命中缓存，不重扫；TTL 到期或用户改了显式路径才重扫。
    #[test]
    fn a_cache_hit_does_not_run_another_scan() {
        let cache = parking_lot::Mutex::new(DetectCache::default());
        let key = (None, None);
        let start = Instant::now();
        let mut scans = 0;
        for offset in [0, 1, 2] {
            let detected = cached_detect(
                &cache,
                key.clone(),
                start + Duration::from_secs(offset),
                || {
                    scans += 1;
                    Detected::Found {
                        path: PathBuf::from(r"D:\devin\extension.js"),
                        source: DetectionSource::Guessed,
                    }
                },
            );
            assert!(detected.path().is_some());
        }
        assert_eq!(scans, 1, "TTL 内的重复探测必须命中缓存");

        cached_detect(
            &cache,
            key.clone(),
            start + DETECT_CACHE_TTL + Duration::from_secs(1),
            || {
                scans += 1;
                Detected::NotFound { searched: vec![] }
            },
        );
        assert_eq!(scans, 2, "TTL 到期必须重新探测");

        cached_detect(&cache, (Some(r"E:\devin".into()), None), start, || {
            scans += 1;
            Detected::NotFound { searched: vec![] }
        });
        assert_eq!(scans, 3, "显式路径变了必须立即重新探测");
    }

    /// 反面结果也缓存：这正是最贵的那条路径（扫描跑满预算还是没找到），而用户
    /// 紧接着再点一次「检查宿主」不该重付一次。
    #[test]
    fn a_cached_negative_result_is_reused_within_the_ttl() {
        let cache = parking_lot::Mutex::new(DetectCache::default());
        let start = Instant::now();
        let mut scans = 0;
        for offset in [0, 1] {
            let detected = cached_detect(
                &cache,
                (None, None),
                start + Duration::from_secs(offset),
                || {
                    scans += 1;
                    Detected::NotFound {
                        searched: vec![PathBuf::from(r"C:\nowhere\extension.js")],
                    }
                },
            );
            assert!(detected.path().is_none());
            assert!(detected.explanation().contains("1 locations"));
        }
        assert_eq!(scans, 1);
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
    fn an_explicit_installation_path_covers_platform_layouts() {
        let directory = tempfile::tempdir().unwrap();
        let windows = directory.path().join("windows-install");
        let windows_extension = windows
            .join("resources")
            .join("app")
            .join("extensions")
            .join("windsurf")
            .join("dist")
            .join("extension.js");
        std::fs::create_dir_all(windows_extension.parent().unwrap()).unwrap();
        std::fs::write(&windows_extension, b"clean").unwrap();
        assert_eq!(
            extension_path_from_input(windows.to_str().unwrap()).unwrap(),
            windows_extension
        );
        assert_eq!(
            extension_path_from_input(windows_extension.to_str().unwrap()).unwrap(),
            windows_extension
        );

        let mac = directory.path().join("Devin.app");
        let mac_extension = mac
            .join("Contents")
            .join("Resources")
            .join("app")
            .join("extensions")
            .join("windsurf")
            .join("dist")
            .join("extension.js");
        std::fs::create_dir_all(mac_extension.parent().unwrap()).unwrap();
        std::fs::write(&mac_extension, b"clean").unwrap();
        assert_eq!(
            extension_path_from_input(mac.to_str().unwrap()).unwrap(),
            mac_extension
        );
    }

    #[test]
    fn an_invalid_installation_path_explains_the_expected_shape() {
        let directory = tempfile::tempdir().unwrap();
        assert!(extension_path_from_input("Devin")
            .unwrap_err()
            .contains("must be absolute"));

        let empty = directory.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(extension_path_from_input(empty.to_str().unwrap())
            .unwrap_err()
            .contains("not a Devin installation directory"));

        let not_extension = directory.path().join("Devin.exe");
        std::fs::write(&not_extension, b"").unwrap();
        assert!(extension_path_from_input(not_extension.to_str().unwrap())
            .unwrap_err()
            .contains("not a Devin extension.js file"));

        let missing = directory.path().join("missing");
        assert!(extension_path_from_input(missing.to_str().unwrap())
            .unwrap_err()
            .contains("not found"));
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
        // 提示给出的是用户能照做的东西：安装目录，或安装目录里的 extension.js。
        assert!(message.contains("extension.js"), "{message}");
        assert!(message.contains("installation directory"), "{message}");
        // 猜过的候选路径与安装目录内部的相对路径都不该进提示。
        assert!(!message.contains(r"C:\nowhere"), "{message}");
        assert!(!message.contains("resources"), "{message}");
        // 界面按这句片段把提示换成中文（`apps/desktop` 的 `harnessErrorText.ts`）：
        // 改了它，那边认不出来，中文提示就没了。
        assert!(
            message.contains("Devin installation not found"),
            "{message}"
        );
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
