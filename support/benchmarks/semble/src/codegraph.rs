//! Adapter lifecycle for benchmarking an installed CodeGraph distribution.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

use serde::{Deserialize, Serialize};

use crate::case::Suite;

#[derive(Debug)]
pub struct CodeGraphRun {
    pub version: String,
    pub cold_index_ms: f64,
    pub persisted_index_bytes: u64,
    pub adapter: AdapterOutput,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterOutput {
    pub load_ms: f64,
    pub stats: AdapterStats,
    pub tracks: Vec<AdapterTrack>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterStats {
    pub node_count: usize,
    pub edge_count: usize,
    pub file_count: usize,
}

#[derive(Debug, Deserialize)]
pub struct AdapterTrack {
    pub name: String,
    pub queries: Vec<AdapterQuery>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterQuery {
    pub id: String,
    pub query: String,
    pub durations_ms: Vec<f64>,
    pub results: Vec<AdapterResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterResult {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f32,
    pub lines: Option<Vec<usize>>,
}

#[derive(Serialize)]
struct AdapterInput<'a> {
    tracks: [AdapterInputTrack<'a>; 3],
}

#[derive(Serialize)]
struct AdapterInputTrack<'a> {
    name: &'static str,
    queries: Vec<AdapterInputQuery<'a>>,
}

#[derive(Serialize)]
struct AdapterInputQuery<'a> {
    id: &'a str,
    query: &'a str,
}

pub fn benchmark(
    executable: &str,
    adapter_path: &Path,
    source: &Path,
    suite: &Suite,
    repetitions: usize,
    top_k: usize,
) -> Result<CodeGraphRun, String> {
    let executable = locate_executable(executable)?;
    let distribution = distribution_root(&executable)?;
    let node = distribution.join(node_binary());
    let library = distribution.join("lib");
    if !node.is_file() || !library.is_dir() {
        return Err(format!(
            "unsupported CodeGraph installation layout below {}",
            distribution.display()
        ));
    }
    if source.join(".codegraph").exists() {
        command_status(
            Command::new(&executable)
                .args(["uninit", "--force"])
                .arg(source),
            "reset CodeGraph index",
        )?;
    }
    let started = Instant::now();
    command_status(
        Command::new(&executable).arg("init").arg(source),
        "build CodeGraph index",
    )?;
    let cold_index_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let persisted_index_bytes = directory_size(&source.join(".codegraph"))?;
    let version = command_output(
        Command::new(&executable).arg("version"),
        "read CodeGraph version",
    )?;
    let input = AdapterInput {
        tracks: [
            AdapterInputTrack {
                name: "natural_language",
                queries: suite
                    .queries
                    .iter()
                    .map(|query| AdapterInputQuery {
                        id: &query.id,
                        query: &query.query,
                    })
                    .collect(),
            },
            AdapterInputTrack {
                name: "literal",
                queries: suite
                    .queries
                    .iter()
                    .map(|query| AdapterInputQuery {
                        id: &query.id,
                        query: &query.literal_query,
                    })
                    .collect(),
            },
            AdapterInputTrack {
                name: "symbol",
                queries: suite
                    .queries
                    .iter()
                    .map(|query| AdapterInputQuery {
                        id: &query.id,
                        query: &query.symbol_query,
                    })
                    .collect(),
            },
        ],
    };
    let encoded = serde_json::to_vec(&input)
        .map_err(|error| format!("serialize CodeGraph benchmark input: {error}"))?;
    let mut child = Command::new(node)
        .arg("--liftoff-only")
        .arg(adapter_path)
        .arg(library)
        .arg(source)
        .arg(repetitions.to_string())
        .arg(top_k.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start CodeGraph benchmark adapter: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "CodeGraph adapter stdin is unavailable".to_owned())?
        .write_all(&encoded)
        .map_err(|error| format!("write CodeGraph benchmark input: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for CodeGraph benchmark adapter: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "CodeGraph benchmark adapter failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let adapter = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("parse CodeGraph benchmark output: {error}"))?;
    Ok(CodeGraphRun {
        version,
        cold_index_ms,
        persisted_index_bytes,
        adapter,
    })
}

/// CodeGraph 发行目录里的 Node 可执行文件名：Windows 带 `.exe`，类 Unix 不带。
/// 写死 `"node"` 会让整个基准在 Windows 上以"不支持的安装布局"失败。
fn node_binary() -> String {
    format!("node{}", std::env::consts::EXE_SUFFIX)
}

/// 定位 CodeGraph 可执行文件：`which` 在 Windows 上不存在，那里用 `where`。
/// `where` 会把所有匹配逐行列出（还可能是 CRLF），只取第一条非空行。
fn locate_executable(name: &str) -> Result<PathBuf, String> {
    let locator = if cfg!(windows) { "where" } else { "which" };
    let output = match command_output(
        Command::new(locator).arg(name),
        "locate CodeGraph executable",
    ) {
        Ok(output) => output,
        Err(error) => {
            return Err(format!(
                "CodeGraph executable `{name}` is not on PATH: {error}"
            ))
        }
    };
    let path = first_path_line(&output)
        .ok_or_else(|| format!("CodeGraph executable `{name}` is not on PATH"))?;
    fs::canonicalize(path).map_err(|error| format!("resolve CodeGraph executable symlink: {error}"))
}

fn first_path_line(output: &str) -> Option<&str> {
    output.lines().map(str::trim).find(|line| !line.is_empty())
}

fn distribution_root(executable: &Path) -> Result<PathBuf, String> {
    executable
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!(
                "cannot locate distribution root for {}",
                executable.display()
            )
        })
}

fn command_status(command: &mut Command, action: &str) -> Result<(), String> {
    let status = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("{action}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{action} failed with {status}"))
    }
}

fn command_output(command: &mut Command, action: &str) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("{action}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{action} failed with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn directory_size(path: &Path) -> Result<u64, String> {
    let mut total = 0_u64;
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("read {}: {error}", directory.display()))?
        {
            let entry = entry.map_err(|error| format!("read directory entry: {error}"))?;
            let metadata = entry
                .metadata()
                .map_err(|error| format!("inspect {}: {error}", entry.path().display()))?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_node_binary_carries_this_platforms_extension() {
        assert_eq!(
            node_binary(),
            format!("node{}", std::env::consts::EXE_SUFFIX)
        );
        #[cfg(windows)]
        assert_eq!(node_binary(), "node.exe");
    }

    /// `where` 会把所有匹配逐行列出，还可能带 CRLF 和空行。
    #[test]
    fn only_the_first_non_empty_locator_line_is_used() {
        assert_eq!(
            first_path_line("C:\\Windows\\System32\\cmd.exe\r\nC:\\Other\\cmd.exe\r\n"),
            Some("C:\\Windows\\System32\\cmd.exe")
        );
        assert_eq!(first_path_line("\n\n/usr/bin/sh\n"), Some("/usr/bin/sh"));
        assert_eq!(first_path_line("   \r\n"), None);
        assert_eq!(first_path_line(""), None);
    }

    /// 定位器换成了 `where`（Windows）：用两个平台都必然存在的程序实跑一次，
    /// 确认能拿到绝对路径而不是空。
    #[test]
    fn the_locator_resolves_a_program_every_platform_ships() {
        let program = if cfg!(windows) { "cmd" } else { "sh" };
        let path = locate_executable(program)
            .unwrap_or_else(|error| panic!("{program} must be locatable: {error}"));
        assert!(path.is_absolute(), "{}", path.display());
    }

    #[test]
    fn a_program_that_does_not_exist_is_reported_by_name() {
        let error = locate_executable("codegraph-that-does-not-exist")
            .expect_err("a missing executable must not resolve");
        assert!(error.contains("codegraph-that-does-not-exist"), "{error}");
    }
}
