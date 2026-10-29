import { useEffect, useMemo, useState } from "react";
import controls from "../../shared/ui/Controls.module.scss";
import { Checkbox } from "../../shared/ui/Checkbox";
import { SearchInput } from "../../shared/ui/SearchInput";
import styles from "./CursorSettings.module.scss";

export function ModelDiscoveryPicker({ models, existing, busy, onClose, onConfirm }: {
  models: string[];
  existing: Set<string>;
  busy: boolean;
  onClose: () => void;
  onConfirm: (modelIds: string[]) => void;
}) {
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<Set<string>>(() => new Set());

  useEffect(() => {
    setQuery("");
    setSelected(new Set());
  }, [models]);

  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return needle ? models.filter((modelId) => modelId.toLowerCase().includes(needle)) : models;
  }, [models, query]);
  const selectedIds = [...selected];
  const submitLabel = selectedIds.length === 1
    ? t("选择这个模型")
    : t("添加 {count} 个模型", { count: selectedIds.length });

  return <section className={styles.discoveryPicker} aria-label={t("选择模型")}>
    <div className={styles.discoveryHeader}>
      <div>
        <strong>{t("选择模型")}</strong>
        <p>{t("只添加你勾选的模型。")}</p>
      </div>
      <button type="button" className={styles.discoveryClose} disabled={busy} onClick={onClose}>{t("收起")}</button>
    </div>
    <SearchInput
      className={styles.discoverySearch}
      value={query}
      onValueChange={setQuery}
      ariaLabel={t("搜索模型")}
      placeholder={t("搜索模型")}
    />
    <div className={styles.discoveryList} role="group" aria-label={t("可添加的模型")}>
      {visible.map((modelId) => {
        const configured = existing.has(modelId);
        return <div className={styles.discoveryRow} key={modelId}>
          <Checkbox
            checked={selected.has(modelId)}
            disabled={configured || busy}
            label={modelId}
            onChange={(checked) => setSelected((current) => {
              const next = new Set(current);
              if (checked) next.add(modelId);
              else next.delete(modelId);
              return next;
            })}
          />
          {configured && <span className={styles.discoveryExisting}>{t("已存在")}</span>}
        </div>;
      })}
      {visible.length === 0 && <p className={styles.discoveryEmpty}>{t("没有匹配的模型")}</p>}
    </div>
    <div className={styles.discoveryActions}>
      <button
        type="button"
        className={controls.primary}
        disabled={busy || selectedIds.length === 0}
        onClick={() => onConfirm(selectedIds)}
      >
        {busy ? t("处理中…") : submitLabel}
      </button>
    </div>
  </section>;
}
