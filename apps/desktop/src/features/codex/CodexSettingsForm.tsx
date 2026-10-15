import type { CodexSettings, Model } from "../../shared/api";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { ModelSelect, type ModelSelectOption } from "../../shared/ui/ModelSelect";
import { Switch } from "../../shared/ui/Switch";
import { modelProviderName } from "../../shared/utils/modelProvider";
import { claudeIcon, openAiIcon } from "../../shared/ui/icons";
import styles from "./CodexSettingsPage.module.scss";

export function CodexSettingsForm({ draft, models, locked, onChange }: {
  draft: CodexSettings; models: Model[]; locked: boolean; onChange: (settings: CodexSettings) => void;
}) {
  const options: ModelSelectOption[] = models.map(model => ({ value: model.model_hash,
    label: model.display_name || model.model_id, group: modelProviderName(model),
    icon: model.type === "anthropic" ? claudeIcon : openAiIcon }));
  const selected = options.filter(option => draft.model_hashes.includes(option.value));
  return <div className={styles.form}>
    {locked && <p className={styles.hint}>{t("撤销接管后才能修改设置")}</p>}
    <div className={styles.switchRow}><span>{t("启用 Codex 网关")}</span><Switch label={t("启用 Codex 网关")} checked={draft.enabled} disabled={locked} onChange={enabled => onChange({ ...draft, enabled })} /></div>
    <FormField label={t("可用模型")} hint={t("共享模型库中的 API 模型。请先为每个模型填写真实上下文窗口。")}> 
      <ModelSelect mode="multiple" label={t("选择 Codex 可用模型")} options={options} value={draft.model_hashes} disabled={locked}
        onChange={model_hashes => onChange({ ...draft, model_hashes, default_model_hash: model_hashes.includes(draft.default_model_hash ?? "") ? draft.default_model_hash : model_hashes[0] ?? null })} />
    </FormField>
    <FormField label={t("默认模型")}><ModelSelect mode="single" label={t("选择 Codex 默认模型")} options={selected} value={draft.default_model_hash ?? ""} disabled={locked || selected.length === 0} onChange={default_model_hash => onChange({ ...draft, default_model_hash })} /></FormField>
    <FormField label={t("Codex 配置目录")} hint={t("留空使用 CODEX_HOME 或用户目录下的 .codex。只在点击应用时修改 config.toml。")}> 
      <TextInput aria-label={t("Codex 配置目录")} disabled={locked} value={draft.home} placeholder={t("自动检测配置目录")} onChange={event => onChange({ ...draft, home: event.target.value })} />
    </FormField>
    <FormField label={t("Codex 网关端口")}><TextInput aria-label={t("Codex 网关端口")} disabled={locked} type="number" min={1} max={65535} value={draft.port} onChange={event => onChange({ ...draft, port: Number(event.target.value) })} /></FormField>
  </div>;
}
