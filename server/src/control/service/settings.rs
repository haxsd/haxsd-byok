//! 控制面的设置与存储域：端口、代理、界面偏好、压缩、定价与存储统计。

use crate::{
    control::settings::OutboundStatus,
    store::{
        CommitSettings, CompactionSettings, DesktopSettings, DesktopSettingsPatch,
        ModelTokenPricing, PortSettings, ProxySettings, ProxySettingsInput, StatisticsStorage,
        StatisticsStorageScope, TabSettings, TokenPricingSettings,
    },
    Result,
};

use super::ControlService;

impl ControlService {
    pub async fn ports(&self) -> Result<PortSettings> {
        self.store.port_settings().await
    }

    pub async fn set_ports(&self, settings: PortSettings) -> Result<PortSettings> {
        self.store.set_port_settings(settings).await?;
        Ok(settings)
    }

    pub async fn statistics_storage(&self) -> Result<StatisticsStorage> {
        self.store.statistics_storage().await
    }

    /// 开始一次清理：立刻返回"已在清理"，不在这里等全部批次跑完。
    ///
    /// 删除本身跑在 store 的后台任务里（每批一个事务、批间放掉写锁），接口只负责把任务
    /// 启动起来并把此刻的进度带回去——库大的时候几百个批次要跑几十秒，同步等的那个请求
    /// 会先撞上桌面端 15 秒超时，用户看到的是"清理失败"，而库其实正在被清。
    pub async fn start_clear_statistics_storage(
        &self,
        scope: StatisticsStorageScope,
    ) -> Result<StatisticsStorage> {
        self.store.start_clear_statistics_storage(scope).await
    }

    /// 请求压缩数据库；执行由维护循环在确认没有活跃 Agent 请求后接手。
    pub async fn start_storage_compaction(&self) -> Result<StatisticsStorage> {
        self.store.start_storage_compaction().await
    }

    pub async fn proxy_settings(&self) -> Result<ProxySettings> {
        self.store.proxy_settings().await
    }

    /// 出网此刻实际走哪条路。设置页用它说明"正在绕过系统代理"这类与配置不一致的状态。
    pub async fn outbound_status(&self) -> Result<OutboundStatus> {
        Ok(OutboundStatus {
            system_proxy_bypassed: crate::network::system_proxy_bypassed(),
        })
    }

    pub async fn set_proxy_settings(&self, settings: ProxySettingsInput) -> Result<ProxySettings> {
        if settings.mode.is_custom() {
            let local_proxy_port = match self.cursor_harness.proxy_port().await {
                Some(port) => port,
                None => self.store.port_settings().await?.proxy_port,
            };
            crate::network::reject_self_proxy(&settings.address, local_proxy_port)?;
        }
        let settings = self.store.set_proxy_settings(settings).await?;
        self.clients.settings_saved().await;
        Ok(settings)
    }

    pub async fn tab_settings(&self) -> Result<TabSettings> {
        self.store.tab_settings().await
    }

    pub async fn set_tab_settings(&self, settings: TabSettings) -> Result<TabSettings> {
        self.cursor_harness.set_tab_settings(settings).await
    }

    pub async fn desktop_settings(&self) -> Result<DesktopSettings> {
        self.store.desktop_settings().await
    }

    /// 桌面设置按字段更新：界面每次只提交改动的字段，合并由 store 在写锁内完成。
    pub async fn update_desktop_settings(
        &self,
        patch: DesktopSettingsPatch,
    ) -> Result<DesktopSettings> {
        self.store.update_desktop_settings(patch).await
    }

    pub async fn commit_settings(&self) -> Result<CommitSettings> {
        self.store.commit_settings().await
    }

    pub async fn set_commit_settings(&self, settings: CommitSettings) -> Result<CommitSettings> {
        self.store.set_commit_settings(settings).await
    }

    pub async fn compaction_settings(&self) -> Result<CompactionSettings> {
        self.store.compaction_settings().await
    }

    pub async fn set_compaction_settings(
        &self,
        settings: CompactionSettings,
    ) -> Result<CompactionSettings> {
        self.store.set_compaction_settings(settings).await
    }

    pub async fn pricing_settings(&self) -> Result<TokenPricingSettings> {
        self.store.pricing_settings().await
    }

    /// 保存一个模型的价格规则，返回合并后的完整规则表。
    pub async fn set_model_pricing(
        &self,
        model_hash: &str,
        pricing: ModelTokenPricing,
    ) -> Result<TokenPricingSettings> {
        self.store.set_model_pricing(model_hash, pricing).await
    }

    /// 删除一个模型的价格规则，返回合并后的完整规则表。
    pub async fn remove_model_pricing(&self, model_hash: &str) -> Result<TokenPricingSettings> {
        self.store.remove_model_pricing(model_hash).await
    }
}
