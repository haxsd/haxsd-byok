//! 控制面的 Devin 域：状态、设置与正在监听的端口。

use crate::{
    devin::{
        gateway::{DevinBoundPorts, DevinListening},
        DevinSettings,
    },
    Result,
};

use super::{ControlService, DevinStatus};

fn devin_ready(settings: &DevinSettings, bound_ports: Option<DevinBoundPorts>) -> bool {
    settings.enabled
        && bound_ports.is_some_and(|ports| {
            ports.api == settings.api_port
                && ports.inference == settings.inference_port
                && ports.local_api == settings.local_api_port
        })
}

impl ControlService {
    /// Counts the calls Devin produced, which is how the UI knows whether a real
    /// conversation has travelled through the gateway yet.
    pub async fn devin_status(&self, bound_ports: Option<DevinBoundPorts>) -> Result<DevinStatus> {
        let settings = self.store.devin_settings().await?;
        let calls = self.store.devin_call_count().await?;
        let last_call = self.store.latest_devin_call().await?;
        let ready = devin_ready(&settings, bound_ports);
        Ok(DevinStatus {
            enabled: settings.enabled,
            listening: bound_ports.is_some(),
            ready,
            bound_ports,
            calls,
            last_call_success: last_call.as_ref().map(|(status, _)| status == "completed"),
            last_call_at_ms: last_call.map(|(_, at)| at),
        })
    }

    pub async fn devin_settings(&self) -> Result<DevinSettings> {
        self.store.devin_settings().await
    }

    /// 保存 Devin 设置。端口占用在这里拦下：网关只在启动时绑一次端口，没有重试与
    /// 回退，被占用时只会表现为"已保存、未监听、待重启"，重启也救不回来。
    pub async fn set_devin_settings(&self, settings: DevinSettings) -> Result<DevinSettings> {
        super::super::devin_config::save_settings(
            &self.store,
            settings,
            self.devin_bound_ports.bound_ports(),
        )
        .await
    }

    /// 本进程正在监听的 Devin 端口。宿主补丁要指向真正在监听的端口，
    /// 否则客户端会打到没人接的地址上。
    pub fn devin_bound_ports(&self) -> DevinListening {
        self.devin_bound_ports.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devin_readiness_requires_enabled_and_exact_ports() {
        let settings = DevinSettings {
            enabled: true,
            ..Default::default()
        };
        let exact = DevinBoundPorts {
            api: settings.api_port,
            inference: settings.inference_port,
            local_api: settings.local_api_port,
        };

        assert!(devin_ready(&settings, Some(exact)));
        assert!(!devin_ready(&DevinSettings::default(), Some(exact)));
        assert!(!devin_ready(&settings, None));

        for bound in [
            DevinBoundPorts {
                api: exact.api + 1,
                ..exact
            },
            DevinBoundPorts {
                inference: exact.inference + 1,
                ..exact
            },
            DevinBoundPorts {
                local_api: exact.local_api + 1,
                ..exact
            },
        ] {
            assert!(
                !devin_ready(&settings, Some(bound)),
                "a process still holding the previous ports is not ready"
            );
        }
    }
}
