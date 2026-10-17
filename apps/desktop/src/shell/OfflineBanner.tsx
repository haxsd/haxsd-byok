import { memo, useEffect, useState } from "react";
import { appStore, useAppStoreSelector } from "../shared/store/appStore";
import { selectOffline } from "../shared/store/selectors";
import { Icon } from "../shared/ui/Icon";
import { alertCircleIcon, refreshIcon } from "../shared/ui/icons";
import controls from "../shared/ui/Controls.module.scss";
import { retryDelayMs, shouldAutoRetry, windowVisibility } from "./offlineRetry";
import styles from "./OfflineBanner.module.scss";

/**
 * 本地管理服务连不上时的常驻提示。
 *
 * 以前这种情况只弹一条提示，之后界面就一直显示 0：调用数 0、模型 0、Token 0——
 * 看起来像「还没有数据」，而不是「没连上」。服务挂了和没有记录是两件事，必须区分，
 * 所以这里给一条不会自己消失的说明，并在后台持续重试。
 *
 * 重试本身是递归 setTimeout（上一拍走完才排下一拍，慢的一批不会被下一拍重复发），
 * 走 `offlineRetry.ts` 的判据：窗口最小化或没有焦点时不发请求，间隔 3s→10s→30s 退避。
 */
export const OfflineBanner = memo(function OfflineBanner() {
  // 只订阅 offline 一个字段：外壳上任何一次写入（busy、error、各种状态刷新）都不该
  // 让这条横幅重渲染。
  const offline = useAppStoreSelector(selectOffline);
  const [attempts, setAttempts] = useState(0);
  const [retrying, setRetrying] = useState(false);

  useEffect(() => {
    if (!offline) {
      setAttempts(0);
      return;
    }
    let cancelled = false;
    let timer = 0;
    let attempt = 0;
    const schedule = () => {
      if (cancelled) return;
      timer = window.setTimeout(run, retryDelayMs(attempt));
    };
    const run = () => {
      if (cancelled) return;
      // 用户没在看这个窗口：这一拍不发请求，只把下一次排上，等他切回来再补。
      if (!shouldAutoRetry(windowVisibility(document))) {
        attempt += 1;
        schedule();
        return;
      }
      setAttempts((count) => count + 1);
      void appStore.refresh().finally(() => {
        attempt += 1;
        schedule();
      });
    };
    schedule();
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
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
});
