import type { CodexStatus } from "../../shared/api";
import { TitledCard } from "../../shared/ui/TitledCard";
import { codexConflictRows } from "./codexConnectionView";
import styles from "./CodexSettingsPage.module.scss";

/**
 * 受管字段被改过、或路由被别人占着时的卡片。
 *
 * 以前这种情况只有一句「配置冲突」：看不出是哪一项、被改成了什么，也不知道该点哪个
 * 按钮（撤销还是应用）。这里逐条列出「接管时 X → 现在 Y」，并给出两步做法。
 */
export function CodexConflictCard({ status }: { status: CodexStatus | null }) {
  if (!status || (!status.config_conflict && status.conflicts.length === 0)) return null;
  const rows = codexConflictRows(status.conflicts);
  return <TitledCard title={status.config_conflict ? t("配置冲突") : t("受管字段被改过")}>
    <div className={styles.notes}>
      {status.config_conflict && <p>
        {t("目标目录里已有其他 provider，本机网关没能接管：Codex 现在用的是别人写的配置。处理掉占用后，再点「应用到 Codex」。")}
      </p>}
      {rows.length > 0 && <p>{t("这些受管字段在接管之后被改过：")}</p>}
      {rows.length > 0 && <ul className={styles.conflictList}>
        {rows.map((row) => <li key={row.key}>
          {t("{field}：接管时 {applied} → 现在 {current}", { field: row.field, applied: row.applied, current: row.current })}
        </li>)}
      </ul>}
      {rows.length > 0 && <p>
        {t("受管字段被改过时，点「撤销接管」会把它们还原成接管前的值；想继续用本机网关，就再点「应用到 Codex」。")}
      </p>}
    </div>
  </TitledCard>;
}
