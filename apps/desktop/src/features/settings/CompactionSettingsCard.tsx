import { useEffect, useState } from "react";
import { api, type CompactionSettings } from "../../shared/api";
import { useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { Select } from "../../shared/ui/Select";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import styles from "./SettingsPage.module.scss";

type Draft = {
  auto: boolean;
  summary_model_hash: string;
  summary_output_tokens: string;
  reserve_percent: string;
  retention_percent: string;
  max_retained_tokens: string;
};

function draftOf(settings: CompactionSettings): Draft {
  return {
    auto: settings.auto,
    summary_model_hash: settings.summary_model_hash,
    summary_output_tokens: settings.summary_output_tokens === null ? "" : String(settings.summary_output_tokens),
    reserve_percent: settings.reserve_percent === null ? "" : String(settings.reserve_percent),
    retention_percent: settings.retention_percent === null ? "" : String(settings.retention_percent),
    max_retained_tokens: settings.max_retained_tokens === null ? "" : String(settings.max_retained_tokens),
  };
}

function optionalInteger(value: string, name: string): number | null {
  const trimmed = value.trim();
  if (!trimmed) return null;
  const parsed = Number(trimmed);
  if (!Number.isInteger(parsed) || parsed <= 0) throw new Error(t("{name} 必须是正整数", { name }));
  return parsed;
}

function optionalPercent(value: string, name: string): number | null {
  const parsed = optionalInteger(value, name);
  if (parsed !== null && parsed > 50) throw new Error(t("{name} 必须在 1–50 之间", { name }));
  return parsed;
}

/** Context-compaction policy: proactive trigger, retention budgets, summary model. */
export function CompactionSettingsCard({ visible }: { visible: boolean }) {
  const { models } = useAppStore();
  const [settings, setSettings] = useState<CompactionSettings | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!visible) return;
    let active = true;
    void api.compactionSettings().then(loaded => {
      if (!active) return;
      setSettings(loaded);
      setDraft(draftOf(loaded));
    }).catch(cause => {
      if (active) setError(String(cause));
    });
    return () => { active = false; };
  }, [visible]);

  const edit = () => {
    if (!settings) return;
    setDraft(draftOf(settings));
    setEditing(true);
    setStatus(null);
    setError(null);
  };
  const cancel = () => {
    if (settings) setDraft(draftOf(settings));
    setEditing(false);
    setError(null);
  };
  const save = async () => {
    if (!draft) return;
    setError(null);
    try {
      const next: CompactionSettings = {
        auto: draft.auto,
        summary_model_hash: draft.summary_model_hash,
        summary_output_tokens: optionalInteger(draft.summary_output_tokens, t("摘要输出上限")),
        reserve_percent: optionalPercent(draft.reserve_percent, t("预留比例")),
        retention_percent: optionalPercent(draft.retention_percent, t("保留比例")),
        max_retained_tokens: optionalInteger(draft.max_retained_tokens, t("保留上限")),
      };
      setSaving(true);
      const saved = await api.setCompactionSettings(next);
      setSettings(saved);
      setDraft(draftOf(saved));
      setEditing(false);
      setStatus(t("上下文压缩设置已保存"));
    } catch (cause) {
      setError(String(cause instanceof Error ? cause.message : cause));
    } finally {
      setSaving(false);
    }
  };

  const modelName = (hash: string) => models.find(model => model.model_hash === hash)?.display_name ?? hash;
  const summaryModel = settings?.summary_model_hash ? modelName(settings.summary_model_hash) : t("跟随会话模型");

  return <TitledCard title={t("上下文压缩")} action={editing ? <div className={styles.cardActions}>
    <Button size="small" disabled={saving} onClick={cancel}>{t("取消")}</Button>
    <Button size="small" variant="primary" disabled={saving || !draft} onClick={() => void save()}>{saving ? t("保存中…") : t("保存")}</Button>
  </div> : settings ? <Button size="small" onClick={edit}>{t("编辑")}</Button> : undefined}>
    <div className={styles.portSettings}>
      {editing && draft ? <>
        <div className={styles.settingRow}>
          <div><strong>{t("自动压缩")}</strong><small>{t("每次模型请求前检查压力；关闭后只保留溢出恢复与手动总结。")}</small></div>
          <Switch checked={draft.auto} disabled={saving} label={t("自动压缩")} onChange={value => setDraft({ ...draft, auto: value })} />
        </div>
        <div className={styles.portFields}>
          <FormField label={t("预留比例（%）")} hint={t("留空使用默认 10%。")}>
            <TextInput type="number" min={1} max={50} value={draft.reserve_percent} onChange={event => setDraft({ ...draft, reserve_percent: event.target.value })} />
          </FormField>
          <FormField label={t("保留比例（%）")} hint={t("留空使用默认 20%。")}>
            <TextInput type="number" min={1} max={50} value={draft.retention_percent} onChange={event => setDraft({ ...draft, retention_percent: event.target.value })} />
          </FormField>
          <FormField label={t("保留上限（tokens）")} hint={t("留空表示只按比例；填写后限制逐字保留的绝对上限。")}>
            <TextInput type="number" min={1} value={draft.max_retained_tokens} onChange={event => setDraft({ ...draft, max_retained_tokens: event.target.value })} />
          </FormField>
          <FormField label={t("摘要输出上限（tokens）")} hint={t("留空使用默认 8192，并受摘要模型自身上限约束。")}>
            <TextInput type="number" min={1} value={draft.summary_output_tokens} onChange={event => setDraft({ ...draft, summary_output_tokens: event.target.value })} />
          </FormField>
          <FormField label={t("摘要模型")} hint={t("留空时复用会话当前模型。独立模型可降低压缩成本。")}>
            <Select
              value={draft.summary_model_hash}
              ariaLabel={t("摘要模型")}
              options={[
                { value: "", label: t("跟随会话模型") },
                ...models.map(model => ({ value: model.model_hash, label: model.display_name })),
              ]}
              onChange={value => setDraft({ ...draft, summary_model_hash: value })}
            />
          </FormField>
        </div>
      </> : settings ? <>
        <div className={styles.portValue}><strong>{t("自动压缩")}</strong><span>{settings.auto ? t("已开启") : t("已关闭")}</span></div>
        <div className={styles.portValue}><strong>{t("摘要模型")}</strong><span>{summaryModel}</span></div>
        <div className={styles.portValue}><strong>{t("预留 / 保留")}</strong><span>{settings.reserve_percent ?? 10}% / {settings.retention_percent ?? 20}%</span></div>
        <div className={styles.portValue}><strong>{t("保留上限")}</strong><span>{settings.max_retained_tokens === null ? t("未设置") : `${settings.max_retained_tokens} tokens`}</span></div>
      </> : <small>{t("读取中…")}</small>}
      {status && <small role="status">{status}</small>}
      {error && <small role="alert">{error}</small>}
    </div>
  </TitledCard>;
}
