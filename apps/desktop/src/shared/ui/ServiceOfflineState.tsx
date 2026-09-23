import { appStore } from "../store/appStore";
import { Button } from "./Button";
import { EmptyState } from "./EmptyState";
import { alertCircleIcon } from "./icons";

/**
 * 服务不可达时的页面状态。
 *
 * 没有它，一个连不上的服务会伪装成「这里什么都没有」：模型库显示「库还是空的」、
 * 调用记录显示「没有符合条件的调用」，两句都是假话。离线与空是两件事。
 */
export function ServiceOfflineState({ compact = false }: { compact?: boolean }) {
  return <EmptyState
    icon={alertCircleIcon}
    tone="bad"
    compact={compact}
    title={t("服务未连接")}
    description={t("本地管理服务暂时不可用。它通常在应用重启后的几秒内恢复，恢复后这一页会自动刷新。")}
    actions={<Button size="small" onClick={() => void appStore.refresh()}>{t("立即重试")}</Button>}
  />;
}
