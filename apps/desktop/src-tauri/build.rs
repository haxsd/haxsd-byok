fn main() {
    let manifest = tauri_build::AppManifest::new().commands(&["open_terminal_with_command"]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("failed to build Tauri application")
}
