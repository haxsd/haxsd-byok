use std::path::{Path, PathBuf};

fn main() {
    let manifest = tauri_build::AppManifest::new()
        .commands(&["open_cursor_ca_install_terminal", "open_log_directory"]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("failed to build Tauri application");

    place_webview2_loader_for_tests();
}

/// 把 `WebView2Loader.dll` 放到 `deps/`，否则这台机器上的 `cargo test` 一条用例都跑不到。
///
/// GNU 目标下 tauri-build 只把它复制到 `target/<profile>/`，而测试二进制在
/// `target/<profile>/deps/`：Windows 先找 exe 自己的目录，找不到就以
/// `0xC0000135 (STATUS_DLL_NOT_FOUND)` 直接退出（`cargo test` 报 exit code -1073741515），
/// 用例的成败完全看不出来。手动拷一份能绕过，但那是仓库外的步骤——这里在构建时顺手做完。
///
/// 失败只告警：DLL 缺失只影响测试目标，应用二进制照常。
fn place_webview2_loader_for_tests() {
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    // `target/<profile>/build/<package>-<hash>/out` → `target/<profile>`
    let Some(profile_dir) = Path::new(&out_dir).ancestors().nth(3) else {
        return;
    };
    let loader: PathBuf = profile_dir.join("WebView2Loader.dll");
    if !loader.is_file() {
        // 非 Windows、或 tauri-build 没找到 webview2-com-sys 的产物。
        return;
    }
    let destination = profile_dir.join("deps").join("WebView2Loader.dll");
    if let Err(error) = std::fs::copy(&loader, &destination) {
        println!(
            "cargo:warning=cannot copy {} to {}: {error}",
            loader.display(),
            destination.display()
        );
    }
}
