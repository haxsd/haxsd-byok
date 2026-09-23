import { useEffect, useState } from "react";
import { appStore, useAppStore } from "../shared/store/appStore";
import { Icon } from "../shared/ui/Icon";
import { alertCircleIcon, refreshIcon } from "../shared/ui/icons";
import controls from "../shared/ui/Controls.module.scss";
import styles from "./OfflineBanner.module.scss";

/** 重试间隔：服务通常在应用重启或端口变更后的几秒内回来。 */
const RETRY_INTERVAL_MS = 3_000;

/**
 * 本地管理服务连不上时的常驻提示。
 *
 * 以前这种情况只弹一条提示，之后界面就一直显示 0：调用数 0、模型 0、Token 0——
 * 看起来像「还没有数据」，而不是「没连上」。服务挂了和没有记录是两件事，必须区分，
 * 所以这里给一条不会自己消失的说明，并在后台持续重试。
 */
export function OfflineBanner() {
  const { offline } = useAppStore();
  const [attempts, setAttempts] = useState(0);
  const [retrying, setRetrying] = useState(false);

  useEffect(() => {
    if (!offline) {
      setAttempts(0);
      return;
    }
    const timer = window.setInterval(() => {
      setAttempts((count) => count + 1);
      void appStore.refresh();
    }, RETRY_INTERVAL_MS);
    return () => window.clearInterval(timer);
  }, [offline]);

  if (!offline) return null;

  return <div className={styles.root} role="alert">
    <Icon icon={alertCircleIcon} size="1.2em" />
    <div className={styles.text}>
      <strong>{t("无法连接本地管理服务")}</strong>
      <small>{attempts === 0
        ? t("正在自动重试；服务通常在应用重启后的几秒内可用。")
        : t("已自动重试 {count} 次，仍在等待服务恢复。", { count: attempts })}</small>
    </div>
    <button
      type="button"
      className={styles.retry}
      disabled={retrying}
      onClick={() => {
        setRetrying(true);
        void appStore.refresh().finally(() => setRetrying(false));
      }}
    >
      <Icon className={retrying ? controls.spin : undefined} icon={refreshIcon} size="1.1em" />
      {retrying ? t("重试中…") : t("立即重试")}
    </button>
  </div>;
}
