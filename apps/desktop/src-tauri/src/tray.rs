use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    App,
};

#[cfg(target_os = "windows")]
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};

use crate::desktop::open_main_window;

const OPEN_MENU_ID: &str = "tray-open";
const QUIT_MENU_ID: &str = "tray-quit";

/// 托盘菜单文案：`zh` / `zh-*` 中文，其余系统语言一律英文。
///
/// 托盘在前端加载之前就建好了，走不到前端的 `t()`，于是"打开 haxsd byok / 退出"写死了
/// 简体中文——系统语言是英文（界面全英文）的用户，托盘里却是两条中文。规则与前端
/// `apps/desktop/src/i18n/store.ts` 的 `resolveSystemLocale` 保持一致：`system` 是默认偏好，
/// 不支持的语言回退英文。
///
/// 已知边界：用户在前端显式选过语言时，托盘仍按**系统**语言显示。要让两者一致，只能由
/// 前端在窗口加载后把已翻译的文案回传给壳（新增一条命令 + 更新菜单），见
/// 01-entry-lifecycle/P3-3 的 note。
fn tray_labels(locale: &str) -> (&'static str, &'static str) {
    let normalized = locale.to_ascii_lowercase();
    if normalized == "zh" || normalized.starts_with("zh-") {
        ("打开 haxsd byok", "退出")
    } else {
        ("Open haxsd byok", "Quit")
    }
}

/// 系统语言标签（`zh-CN` 这种）；拿不到就回空串（→ 英文文案）。
fn system_locale() -> String {
    #[cfg(windows)]
    if let Some(locale) = windows_locale() {
        return locale;
    }
    #[cfg(not(windows))]
    if let Some(locale) = unix_locale() {
        return locale;
    }
    String::new()
}

#[cfg(windows)]
fn windows_locale() -> Option<String> {
    use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
    // LOCALE_NAME_MAX_LENGTH：BCP-47 名称的最大长度（含结尾 NUL）。
    let mut buffer = [0u16; 85];
    let length = unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), buffer.len() as i32) };
    // 返回含结尾 NUL 的长度；0 表示失败。
    let length = usize::try_from(length).ok()?.checked_sub(1)?;
    Some(String::from_utf16_lossy(&buffer[..length]))
}

#[cfg(not(windows))]
fn unix_locale() -> Option<String> {
    for name in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        // `zh_CN.UTF-8` / `zh_CN` → `zh-CN`
        let locale = value
            .split('.')
            .next()
            .unwrap_or_default()
            .replace('_', "-");
        if !locale.is_empty() {
            return Some(locale);
        }
    }
    None
}

pub fn create(app: &mut App) -> tauri::Result<()> {
    let (open_label, quit_label) = tray_labels(&system_locale());
    let open = MenuItem::with_id(app, OPEN_MENU_ID, open_label, true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, QUIT_MENU_ID, quit_label, true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &separator, &quit])?;

    TrayIconBuilder::with_id("main")
        .icon(tauri::include_image!("./icons/32x32.png"))
        .tooltip("haxsd byok")
        .menu(&menu)
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, event| match event.id().as_ref() {
            OPEN_MENU_ID => {
                let _ = open_main_window(app);
            }
            QUIT_MENU_ID => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            #[cfg(target_os = "windows")]
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = open_main_window(tray.app_handle());
            }

            #[cfg(not(target_os = "windows"))]
            let _ = (tray, event);
        })
        .build(app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::tray_labels;

    /// 托盘菜单按系统语言选文案（与前端 `resolveSystemLocale` 同一条规则：
    /// `zh` / `zh-*` → 中文，其余一律英文）。
    #[test]
    fn the_tray_menu_follows_the_system_language() {
        assert_eq!(tray_labels("zh-CN"), ("打开 haxsd byok", "退出"));
        assert_eq!(tray_labels("zh-TW"), ("打开 haxsd byok", "退出"));
        assert_eq!(tray_labels("zh"), ("打开 haxsd byok", "退出"));
        assert_eq!(tray_labels("en-US"), ("Open haxsd byok", "Quit"));
        assert_eq!(tray_labels("de-DE"), ("Open haxsd byok", "Quit"));
        // 拿不到系统语言时按英文（i18n 规则：不支持的语言回退英文）。
        assert_eq!(tray_labels(""), ("Open haxsd byok", "Quit"));
    }
}
