import type { CodexUiCacheStatus } from "../../shared/api";
import { clientErrorText } from "../harness/harnessErrorText";
import { Button } from "../../shared/ui/Button";
import { TitledCard } from "../../shared/ui/TitledCard";
import { settingsIcon } from "../../shared/ui/icons";
import styles from "./CodexSettingsPage.module.scss";

/**
 * 只在中文界面出现的 Codex 旧版界面说明。
 *
 * 这段文字解释的是客户端官方 Statsig 开关导致的界面差异，不是本产品接管失败；
 * 不把它放进冲突卡，避免用户把“界面回退”当成配置错误去撤销接管。
 */
export function CodexUiCacheNote({ status, busy, error, onRepair }: {
  status: CodexUiCacheStatus | null;
  busy: boolean;
  error: unknown;
  onRepair: () => void;
}) {
  if (!status && !error) return null;
  const canRepair = status?.state === "restorable" || status?.state === "blocked";
  const blocked = status?.state === "blocked";
  return <TitledCard title={t("Codex 旧版界面说明")} icon={settingsIcon}>
    <div className={styles.cacheNote}>
      <p>{t("Codex 的侧栏、语言和 Work/聊天布局由官方 Statsig 开关决定。国内网络下，如果 Codex 启动时连不上官方服务、本机又没有可用缓存，就会回退到旧版界面：没有左侧图标栏、显示英文、进入聊天模式。")}</p>
      <p>{t("本产品会在正常界面出现后备份 Codex 的本地缓存，并在 Codex 完全退出时恢复。首次安装或升级后，请先在代理可用的情况下启动一次 Codex；看到正常界面后再退出，之后代理晚开也能优先使用缓存。")}</p>
      {status && <p className={styles.cacheState}>{cacheStateText(status)}</p>}
      {error ? <p className={styles.error}>{clientErrorText(error)}</p> : null}
      {canRepair && <div className={styles.cacheAction}>
        <Button size="small" disabled={busy} title={blocked ? t("请先完全退出 Codex") : undefined} onClick={onRepair}>
          {busy ? t("恢复中…") : t("恢复界面缓存")}
        </Button>
        {blocked && <span className={styles.cacheHint}>{t("完全退出 Codex 后即可恢复。")}</span>}
      </div>}
    </div>
  </TitledCard>;
}

function cacheStateText(status: CodexUiCacheStatus): string {
  switch (status.state) {
    case "ready": return t("当前已有可用的本地缓存。");
    case "restorable": return t("当前缓存缺失，但已保存的备份可以恢复。");
    case "blocked": return t("Codex 正在运行，无法安全替换它的缓存。请完全退出 Codex 后再恢复。");
    case "missing": return t("当前没有可恢复的缓存。请先开启代理并启动一次 Codex，等正常界面出现后再退出。");
    case "unsupported": return t("当前无法定位 Codex 的界面缓存；请在代理可用时启动一次 Codex。");
  }
}
