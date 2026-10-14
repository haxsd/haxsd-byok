fn main() {
    let manifest = tauri_build::AppManifest::new()
        .commands(&["open_cursor_ca_install_terminal", "open_log_directory"]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("failed to build Tauri application")
}
