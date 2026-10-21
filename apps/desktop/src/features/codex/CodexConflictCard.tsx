import type { CodexStatus } from "../../shared/api";
import { TitledCard } from "../../shared/ui/TitledCard";
import { alertIcon } from "../../shared/ui/icons";
import { codexConflictRows } from "./codexConnectionView";
import styles from "./CodexSettingsPage.module.scss";

/**
 * 受管字段被改过、或路由被别人占着时的卡片。
 *
 * 只在需要处理时出现：标题 + 一行说明 + 字段清单 + 一句怎么做。正常状态下不占页面。
 */
export function CodexConflictCard({ status }: { status: CodexStatus | null }) {
  if (!status || (!status.config_conflict && status.conflicts.length === 0)) return null;
  const rows = codexConflictRows(status.conflicts);
  return <TitledCard
    title={status.config_conflict ? t("配置冲突") : t("受管字段被改过")}
    description={status.config_conflict ? t("路由被其他程序占用") : t("{count} 个字段已变化", { count: rows.length })}
    icon={alertIcon}
  >
    <div className={styles.notes}>
      {status.config_conflict && <p>{t("Codex 现在使用别人写的 provider，处理占用后再应用。")}</p>}
      {rows.length > 0 && <ul className={styles.conflictList}>
        {rows.map((row) => <li key={row.key}>
          {t("{field}：接管时 {applied} → 现在 {current}", { field: row.field, applied: row.applied, current: row.current })}
        </li>)}
      </ul>}
      {rows.length > 0 && <p>{t("撤销接管会还原这些字段；要继续使用本机网关，再应用一次。")}</p>}
    </div>
  </TitledCard>;
}
