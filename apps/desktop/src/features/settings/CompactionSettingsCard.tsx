import { useEffect, useRef, useState } from "react";
import { api, type CompactionSettings, type Model } from "../../shared/api";
import { useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { ModelSelect, type ModelSelectOption } from "../../shared/ui/ModelSelect";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { claudeIcon, openAiIcon } from "../../shared/ui/icons";
import { modelProviderName } from "../../shared/utils/modelProvider";
import { loadWhenVisible } from "../../shared/utils/visibleLoad";
import { loadCompactionSettings } from "./settingsLoaders";
import styles from "./SettingsPage.module.scss";

// Bounds and shipped defaults are shared with `CompactionSettings::validate` on
// the server. Keeping them here as constants is what makes the hints, the input
// `max` attributes and the validation messages agree with the API instead of
// re-stating the numbers at each call site.
export const COMPACTION_SUMMARY_OUTPUT_TOKENS_DEFAULT = 8_192;
export const COMPACTION_RESERVE_PERCENT_DEFAULT = 10;
export const COMPACTION_RETENTION_PERCENT_DEFAULT = 20;
export const COMPACTION_SUMMARY_OUTPUT_TOKENS_MAX = 32_768;
export const COMPACTION_MAX_RETAINED_TOKENS_MAX = 1_000_000;
export const COMPACTION_RESERVE_RETENTION_PERCENT_MAX = 80;
export const COMPACTION_PERCENT_MAX = 50;

export type CompactionDraft = {
  auto: boolean;
  summary_model_hash: string;
  summary_output_tokens: string;
  reserve_percent: string;
  retention_percent: string;
  max_retained_tokens: string;
};

export function compactionDraft(settings: CompactionSettings): CompactionDraft {
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

function boundedInteger(value: string, name: string, max: number): number | null {
  const parsed = optionalInteger(value, name);
  if (parsed !== null && parsed > max) throw new Error(t("{name} 不能超过 {max}", { name, max }));
  return parsed;
}

function optionalPercent(value: string, name: string): number | null {
  const parsed = optionalInteger(value, name);
  if (parsed !== null && parsed > COMPACTION_PERCENT_MAX) throw new Error(t("{name} 必须在 1–50 之间", { name }));
  return parsed;
}

/** Draft -> API body, validating exactly the rules the server enforces. */
export function compactionSettingsFromDraft(draft: CompactionDraft): CompactionSettings {
  const summary_output_tokens = boundedInteger(draft.summary_output_tokens, t("摘要输出上限"), COMPACTION_SUMMARY_OUTPUT_TOKENS_MAX);
  const reserve_percent = optionalPercent(draft.reserve_percent, t("预留比例"));
  const retention_percent = optionalPercent(draft.retention_percent, t("保留比例"));
  const max_retained_tokens = boundedInteger(draft.max_retained_tokens, t("保留上限"), COMPACTION_MAX_RETAINED_TOKENS_MAX);
  // The reserve and the retained tail share the window. Unset fields mean the
  // shipped defaults, so the check is on the values that will actually be used.
  const reserve = reserve_percent ?? COMPACTION_RESERVE_PERCENT_DEFAULT;
  const retention = retention_percent ?? COMPACTION_RETENTION_PERCENT_DEFAULT;
  if (reserve + retention > COMPACTION_RESERVE_RETENTION_PERCENT_MAX) {
    throw new Error(t("预留比例与保留比例之和不能超过 {max}%（留空时分别按 {reserve}% 和 {retention}% 计算）。", {
      max: COMPACTION_RESERVE_RETENTION_PERCENT_MAX,
      reserve: COMPACTION_RESERVE_PERCENT_DEFAULT,
      retention: COMPACTION_RETENTION_PERCENT_DEFAULT,
    }));
  }
  return {
    auto: draft.auto,
    summary_model_hash: draft.summary_model_hash,
    summary_output_tokens,
    reserve_percent,
    retention_percent,
    max_retained_tokens,
  };
}

/**
 * Options for the summary-model picker: the conversation model first, then every
 * configured model grouped by provider. A saved hash whose model is gone is
 * kept as its own entry, because the server falls back to the conversation model
 * only at run time — silently dropping the entry would lose the fact that the
 * user picked something.
 */
export function compactionModelOptions(models: Model[], savedHash: string): ModelSelectOption[] {
  const options: ModelSelectOption[] = [{ value: "", label: t("跟随会话模型"), group: t("摘要模型") }];
  options.push(...models.map(model => ({
    value: model.model_hash,
    label: model.display_name || model.model_id,
    group: modelProviderName(model),
    icon: model.type === "anthropic" ? claudeIcon : openAiIcon,
  })));
  const hash = savedHash.trim();
  if (hash && !models.some(model => model.model_hash === hash)) {
    options.push({ value: hash, label: t("{hash}（已删除，将回退到会话模型）", { hash }), group: t("已删除的摘要模型") });
  }
  return options;
}

export function compactionSummaryModelLabel(settings: CompactionSettings, models: Model[]): string {
  const hash = settings.summary_model_hash.trim();
  if (!hash) return t("跟随会话模型");
  const configured = models.find(model => model.model_hash === hash);
  return configured ? (configured.display_name || configured.model_id) : t("{hash}（已删除，将回退到会话模型）", { hash });
}

/** Read mode rows; exported so the rendered values can be tested without a DOM. */
export function CompactionSettingsRows({ settings, models }: { settings: CompactionSettings; models: Model[] }) {
  return <>
    <div className={styles.portValue}><strong>{t("自动压缩")}</strong><span>{settings.auto ? t("已开启") : t("已关闭")}</span></div>
    <div className={styles.portValue}><strong>{t("摘要模型")}</strong><span>{compactionSummaryModelLabel(settings, models)}</span></div>
    <div className={styles.portValue}><strong>{t("摘要输出上限")}</strong><span>{settings.summary_output_tokens === null ? t("默认 {tokens}", { tokens: COMPACTION_SUMMARY_OUTPUT_TOKENS_DEFAULT }) : `${settings.summary_output_tokens} tokens`}</span></div>
    <div className={styles.portValue}><strong>{t("预留 / 保留")}</strong><span>{settings.reserve_percent ?? COMPACTION_RESERVE_PERCENT_DEFAULT}% / {settings.retention_percent ?? COMPACTION_RETENTION_PERCENT_DEFAULT}%</span></div>
    <div className={styles.portValue}><strong>{t("保留上限")}</strong><span>{settings.max_retained_tokens === null ? t("未设置") : `${settings.max_retained_tokens} tokens`}</span></div>
  </>;
}

/** Edit mode fields; exported so the input bounds can be tested without a DOM. */
export function CompactionSettingsFields({ draft, models, disabled, onChange }: {
  draft: CompactionDraft;
  models: Model[];
  disabled: boolean;
  onChange: (draft: CompactionDraft) => void;
}) {
  const update = (patch: Partial<CompactionDraft>) => onChange({ ...draft, ...patch });
  return <>
    <div className={styles.settingRow}>
      <div><strong>{t("自动压缩")}</strong><small>{t("每次模型请求前检查压力；关闭后只保留溢出恢复与手动总结。")}</small></div>
      <Switch checked={draft.auto} disabled={disabled} label={t("自动压缩")} onChange={value => update({ auto: value })} />
    </div>
    <div className={styles.portFields}>
      <FormField label={t("预留比例（%）")} hint={t("留空使用默认 {percent}%。", { percent: COMPACTION_RESERVE_PERCENT_DEFAULT })}>
        <TextInput type="number" min={1} max={COMPACTION_PERCENT_MAX} value={draft.reserve_percent} disabled={disabled} onChange={event => update({ reserve_percent: event.target.value })} />
      </FormField>
      <FormField label={t("保留比例（%）")} hint={t("留空使用默认 {percent}%。", { percent: COMPACTION_RETENTION_PERCENT_DEFAULT })}>
        <TextInput type="number" min={1} max={COMPACTION_PERCENT_MAX} value={draft.retention_percent} disabled={disabled} onChange={event => update({ retention_percent: event.target.value })} />
      </FormField>
      <FormField label={t("保留上限（tokens）")} hint={t("留空表示只按比例；填写后限制逐字保留的绝对上限。")}>
        <TextInput type="number" min={1} max={COMPACTION_MAX_RETAINED_TOKENS_MAX} value={draft.max_retained_tokens} disabled={disabled} onChange={event => update({ max_retained_tokens: event.target.value })} />
      </FormField>
      <FormField label={t("摘要输出上限（tokens）")} hint={t("留空使用默认 {tokens}，并受摘要模型自身上限约束；这是上限，不是目标长度。自动压缩会按历史规模要求完整交接，明显过短时重试一次。", { tokens: COMPACTION_SUMMARY_OUTPUT_TOKENS_DEFAULT })}>
        <TextInput type="number" min={1} max={COMPACTION_SUMMARY_OUTPUT_TOKENS_MAX} value={draft.summary_output_tokens} disabled={disabled} onChange={event => update({ summary_output_tokens: event.target.value })} />
      </FormField>
    </div>
    <div>
      <ModelSelect
        mode="single"
        label={t("摘要模型")}
        options={compactionModelOptions(models, draft.summary_model_hash)}
        value={draft.summary_model_hash}
        disabled={disabled}
        onChange={value => update({ summary_model_hash: value })}
      />
      <small>{t("留空时复用会话当前模型。独立模型可降低压缩成本。")}</small>
    </div>
  </>;
}

/** Context-compaction policy: proactive trigger, retention budgets, summary model. */
export function CompactionSettingsCard({ visible }: { visible: boolean }) {
  const { models } = useAppStore();
  const [settings, setSettings] = useState<CompactionSettings | null>(null);
  const [draft, setDraft] = useState<CompactionDraft | null>(null);
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const editingRef = useRef(false);
  useEffect(() => { editingRef.current = editing; }, [editing]);

  useEffect(() => {
    let active = true;
    setError(null);
    void loadWhenVisible(visible, loadCompactionSettings).then(loaded => {
      if (!active || !loaded) return;
      setSettings(loaded);
      // The settings page keeps every card mounted so a section switch or a
      // search never discards a draft. Only a fresh read while *not* editing may
      // seed the draft; otherwise the user's unsaved values would be replaced.
      if (!editingRef.current) setDraft(compactionDraft(loaded));
    }).catch(cause => {
      if (active) setError(String(cause));
    });
    return () => { active = false; };
  }, [visible]);

  const edit = () => {
    if (!settings) return;
    setDraft(compactionDraft(settings));
    setEditing(true);
    setStatus(null);
    setError(null);
  };
  const cancel = () => {
    if (settings) setDraft(compactionDraft(settings));
    setEditing(false);
    setError(null);
  };
  const save = async () => {
    if (!draft) return;
    setError(null);
    try {
      setSaving(true);
      const saved = await api.setCompactionSettings(compactionSettingsFromDraft(draft));
      setSettings(saved);
      setDraft(compactionDraft(saved));
      setEditing(false);
      setStatus(t("上下文压缩设置已保存"));
    } catch (cause) {
      setError(String(cause instanceof Error ? cause.message : cause));
    } finally {
      setSaving(false);
    }
  };

  return <TitledCard title={t("上下文压缩")} action={editing ? <div className={styles.cardActions}>
    <Button size="small" disabled={saving} onClick={cancel}>{t("取消")}</Button>
    <Button size="small" variant="primary" disabled={saving || !draft} onClick={() => void save()}>{saving ? t("保存中…") : t("保存")}</Button>
  </div> : settings ? <Button size="small" onClick={edit}>{t("编辑")}</Button> : undefined}>
    <div className={styles.portSettings}>
      {editing && draft
        ? <CompactionSettingsFields draft={draft} models={models} disabled={saving} onChange={setDraft} />
        : settings
          ? <CompactionSettingsRows settings={settings} models={models} />
          : <small>{t("读取中…")}</small>}
      {status && <small role="status">{status}</small>}
      {error && <small role="alert">{error}</small>}
    </div>
  </TitledCard>;
}
