import cursorIconUrl from "../../shared/assets/icons/cursor.svg";
import type { TabMode, TabSettings } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { TextInput } from "../../shared/ui/FormControls";
import { Icon } from "../../shared/ui/Icon";
import { Select } from "../../shared/ui/Select";
import { TitledCard } from "../../shared/ui/TitledCard";
import styles from "./TabSettingsCard.module.scss";

/**
 * TAB 补全的连接方式。
 *
 * 卡片永远展开：点「编辑」应该看到表单，而不是先展开一张折叠的卡片——折叠状态下
 * 编辑出的草稿没有任何可见的落点。加载失败在卡内行内说明，重试就在旁边。
 */
export function TabSettingsCard({
  settings,
  draft,
  editing,
  saving,
  error,
  onDraftChange,
  onEdit,
  onCancel,
  onSave,
  onRetry,
}: {
  settings: TabSettings | null;
  draft: TabSettings;
  editing: boolean;
  saving: boolean;
  error: string | null;
  onDraftChange: (settings: TabSettings) => void;
  onEdit: () => void;
  onCancel: () => void;
  onSave: () => void;
  onRetry: () => void;
}) {
  const modeLabel = (mode: TabMode) => {
    if (mode === "public") return t("使用公益服务");
    if (mode === "direct") return t("直连");
    return t("自定义");
  };
  const action = editing ? (
    <div className={styles.actionGroup}>
      <Button size="small" disabled={saving} onClick={onCancel}>{t("取消")}</Button>
      <Button variant="primary" size="small" disabled={saving} onClick={onSave}>{saving ? t("保存中…") : t("保存")}</Button>
    </div>
  ) : (
    <button type="button" className={styles.headerAction} disabled={!settings} onClick={onEdit}>{t("编辑")}</button>
  );

  return <TitledCard
    title={<div className={styles.title}><Icon src={cursorIconUrl} size="1.1em" /><span>{t("Tab 补全")}</span></div>}
    action={action}
  >
    <div className={styles.content}>
      {editing ? <>
        <div className={styles.row}>
          <div className={styles.description}>
            <strong>{t("TAB 选择")}</strong>
            <small>{t("控制 Cursor TAB 相关接口的连接方式。")}</small>
          </div>
          <div className={styles.control}><Select
            value={draft.mode}
            ariaLabel={t("TAB 选择")}
            options={[
              { value: "public", label: t("使用公益服务") },
              { value: "direct", label: t("直连") },
              { value: "custom", label: t("自定义") },
            ]}
            onChange={(mode) => onDraftChange({ ...draft, mode: mode as TabMode })}
          /></div>
        </div>
        {draft.mode === "custom" && <div className={styles.row}>
          <div className={styles.description}>
            <strong>{t("TAB 服务地址")}</strong>
            <small>{t("原接口路径会追加到此服务地址。")}</small>
          </div>
          <div className={styles.control}><TextInput
            value={draft.address}
            placeholder="https://tab.leokun.cn"
            aria-label={t("TAB 服务地址")}
            onChange={(event) => onDraftChange({ ...draft, address: event.target.value })}
            onKeyDown={(event) => { if (event.key === "Enter") onSave(); }}
          /></div>
        </div>}
      </> : <>
        <div className={styles.row}>
          <strong>{t("TAB 选择")}</strong>
          <span className={styles.value}>{settings ? modeLabel(settings.mode) : error ? t("读取失败") : t("加载中…")}</span>
        </div>
        {settings?.mode === "custom" && <div className={styles.row}>
          <strong>{t("TAB 服务地址")}</strong>
          <span className={styles.value}>{settings.address}</span>
        </div>}
      </>}
      {error && <div className={styles.error} role="alert">
        <span>{t("读取 TAB 设置失败：{error}", { error })}</span>
        <Button size="small" onClick={onRetry}>{t("重试")}</Button>
      </div>}
    </div>
  </TitledCard>;
}
