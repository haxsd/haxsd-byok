//! Claude Desktop 的受管策略探测。
//!
//! 这里只做判断、不改文件：机器/用户策略是否存在。profile 的写入与撤销在 `desktop.rs`。

/// `HKLM\SOFTWARE\Policies\Claude` / `HKCU\SOFTWARE\Policies\Claude` 的只读快照。
#[derive(Clone, Debug, Default)]
pub struct DesktopPolicy {
    pub machine_values: Vec<String>,
    pub user_values: Vec<String>,
}

impl DesktopPolicy {
    #[cfg(windows)]
    pub fn detect() -> Self {
        Self {
            machine_values: policy_values(windows_registry::LOCAL_MACHINE),
            user_values: policy_values(windows_registry::CURRENT_USER),
        }
    }

    #[cfg(not(windows))]
    pub fn detect() -> Self {
        Self::default()
    }

    pub fn machine(&self) -> bool {
        !self.machine_values.is_empty()
    }

    pub fn user(&self) -> bool {
        !self.user_values.is_empty()
    }
}

#[cfg(windows)]
fn policy_values(root: &windows_registry::Key) -> Vec<String> {
    let Ok(key) = root.open("SOFTWARE\\Policies\\Claude") else {
        return Vec::new();
    };
    match key.values() {
        Ok(values) => values.map(|(name, _)| name).collect(),
        Err(_) => Vec::new(),
    }
}
