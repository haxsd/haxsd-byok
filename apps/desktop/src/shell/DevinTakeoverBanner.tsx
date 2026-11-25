import { memo, useEffect } from "react";
import { useNavigate } from "react-router-dom";
import { devinHostRepairable, devinPointsAtGateway } from "../features/devin/devinConnectionView";
import type { DevinHostPorts } from "../shared/api";
import { appStore, useAppStoreSelector } from "../shared/store/appStore";
import { selectOffline } from "../shared/store/selectors";
import { Icon } from "../shared/ui/Icon";
import { alertIcon } from "../shared/ui/icons";
import styles from "./DevinTakeoverBanner.module.scss";

/**
 * Devin 更新后不会保留 extension.js 补丁，设置文件却常常还在。以前只有打开
 * Devin 页才能看到状态；用户先在 Devin 里发现没有自选模型，才知道接管失效。
 *
 * 这里用已经缓存的 Devin 状态做常驻提示，并在窗口重新获得焦点时轻量复查一次。
 * 修复仍在 Devin 页完成：修改 extension.js 前必须先关闭 Devin，页面负责解释
 * 这一步和失败原因。
 */
export const DevinTakeoverBanner = memo(function DevinTakeoverBanner() {
  const offline = useAppStoreSelector(selectOffline);
  const devinStatus = useAppStoreSelector(state => state.devinStatus);
  const host = useAppStoreSelector(state => state.devinHostStatus);
  const navigate = useNavigate();
  const devinEnabled = Boolean(devinStatus?.enabled);

  useEffect(() => {
    if (!devinEnabled) return;
    let lastCheck = 0;
    const check = () => {
      if (document.hidden) return;
      const now = Date.now();
      if (now - lastCheck < 30_000) return;
      lastCheck = now;
      void appStore.refreshDevinStatus();
    };
    window.addEventListener("focus", check);
    document.addEventListener("visibilitychange", check);
    return () => {
      window.removeEventListener("focus", check);
      document.removeEventListener("visibilitychange", check);
    };
  }, [devinEnabled]);

  if (offline || !devinStatus?.enabled || !devinStatus.ready || !host?.configured) {
    return null;
  }
  const ports: DevinHostPorts | null = devinStatus.bound_ports
    ? {
        api_port: devinStatus.bound_ports.api,
        inference_port: devinStatus.bound_ports.inference,
        local_api_port: devinStatus.bound_ports.local_api,
      }
    : null;
  if (!ports || devinPointsAtGateway(host, ports)) return null;

  const repairable = devinHostRepairable(host, ports);
  return <div className={styles.root} role="status">
    <Icon icon={alertIcon} size="1.2em" />
    <div className={styles.text}>
      <strong>{t("Devin 接管需要修复")}</strong>
      <small>{repairable
        ? t("Devin 更新后补丁没有保留；关闭 Devin，然后在 Devin 页点“修复接管”。")
        : t("Devin 的接管配置与当前版本不匹配；请查看详情，必要时更新 haxsd byok。")}</small>
    </div>
    <button type="button" className={styles.action} onClick={() => void navigate("/harness/devin")}>
      {repairable ? t("去修复") : t("查看详情")}
    </button>
  </div>;
});
