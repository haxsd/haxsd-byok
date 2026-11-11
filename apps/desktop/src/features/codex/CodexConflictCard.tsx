import type { CodexStatus } from "../../shared/api";
import { TitledCard } from "../../shared/ui/TitledCard";
import { alertIcon } from "../../shared/ui/icons";
import { codexConflictRows, codexManagedConflictRows } from "./codexConnectionView";
import styles from "./CodexSettingsPage.module.scss";

/**
 * 受管字段被改过、路由被占用、管理员策略覆盖、本机漂移或半写状态时的卡片。
 *
 * 只在需要处理时出现：标题 + 一行说明 + 字段清单 + 一句怎么做。不同冲突类型
 * 必须分开说，不能把所有 `config_conflict` 都说成"别的程序"。
 */
export function CodexConflictCard({ status }: { status: CodexStatus | null }) {
  if (!status) return null;
  const rows = codexConflictRows(status.conflicts);
  const managedRows = codexManagedConflictRows(status.managed_conflicts);
  if (!status.config_conflict && rows.length === 0 && managedRows.length === 0 && !status.managed_error) return null;

  const kind = status.conflict_kind;
  const title = kind === "managed"
    ? t("管理员策略限制")
    : kind === "local_drift"
      ? t("本机路由已变化")
      : kind === "policy_unavailable"
        ? t("无法读取管理员策略")
        : kind === "incomplete_update"
          ? t("上次应用未完成")
          : kind === "unreadable"
            ? t("配置无法读取")
            : status.config_conflict
              ? t("配置冲突")
              : t("受管字段被改过");
  const description = kind === "managed"
    ? t("本地路由由管理员 requirements 覆盖")
    : kind === "local_drift"
      ? t("仍指向本机网关，但模型或目录已变")
      : kind === "policy_unavailable"
        ? t("本地路由状态暂时无法确认")
        : kind === "incomplete_update"
          ? t("配置可能已写入，但最终收据没有保存")
          : kind === "unreadable"
            ? t("配置或收据无法解析")
            : status.config_conflict
              ? t("路由被其他程序占用")
              : t("{count} 个字段已变化", { count: rows.length });
  const showScope = kind === "managed" || kind === "policy_unavailable" || managedRows.length > 0;

  return <TitledCard
    title={title}
    description={description}
    icon={alertIcon}
  >
    <div className={styles.notes}>
      {kind === "managed" && <p>{t("管理员 requirements 要求不同的 provider/目录，本机网关无法覆盖；请调整策略或撤销接管。")}</p>}
      {kind === "local_drift" && <p>{t("Codex 仍指向本机网关，但模型或目录已被改动；撤销接管会收回本机路由。")}</p>}
      {kind === "policy_unavailable" && <p>{t("无法读取管理员 requirements；本地路由状态暂时无法确认；撤销接管仍可用。")}</p>}
      {kind === "incomplete_update" && <p>{t("上次应用没有完成；配置可能已写入，也可能没有；撤销接管会清理这次未完成的应用。")}</p>}
      {kind === "unreadable" && <p>{t("Codex 配置或收据无法解析；请检查配置目录。")}</p>}
      {(kind === "foreign_provider" || (kind === "none" && status.config_conflict)) && <p>{t("Codex 现在使用别人写的 provider，处理占用后再应用。")}</p>}
      {managedRows.length > 0 && <ul className={styles.conflictList}>
        {managedRows.map((row) => <li key={row.key}>
          {t("{field}：管理员要求 {required}，本机发布 {published}", { field: row.field, required: row.required, published: row.published })}
        </li>)}
      </ul>}
      {rows.length > 0 && <ul className={styles.conflictList}>
        {rows.map((row) => <li key={row.key}>
          {t("{field}：接管时 {applied} → 现在 {current}", { field: row.field, applied: row.applied, current: row.current })}
        </li>)}
      </ul>}
      {showScope && <p>{t("仅核对本地系统 requirements；cloud/enterprise 策略未验证。")}</p>}
      {rows.length > 0 && <p>{t("撤销接管会还原这些字段；要继续使用本机网关，再应用一次。")}</p>}
    </div>
  </TitledCard>;
}
