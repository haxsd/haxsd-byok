import type { CodexSettings, Model, PluginModelDescriptor } from "../../shared/api";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { ModelSelect } from "../../shared/ui/ModelSelect";
import { Switch } from "../../shared/ui/Switch";
import { codexModelAvailability } from "./codexModelOptions";
import styles from "./CodexSettingsPage.module.scss";

export function CodexSettingsForm({ draft, models, pluginModels, locked, onChange }: {
  draft: CodexSettings;
  models: Model[];
  /** 模型库里的插件模型：Codex 网关发布不了它们，界面上要说清为什么。 */
  pluginModels: PluginModelDescriptor[];
  locked: boolean;
  onChange: (settings: CodexSettings) => void;
}) {
  const availability = codexModelAvailability(models, pluginModels, draft.model_hashes);
  const selected = availability.options.filter(option => draft.model_hashes.includes(option.value));
  return <div className={styles.form}>
    <div className={styles.settingRow}>
      <div>
        <strong>{t("启用 Codex 网关")}</strong>
        <small>{locked ? t("撤销接管后才能修改") : t("关闭时不监听端口")}</small>
      </div>
      <Switch label={t("启用 Codex 网关")} checked={draft.enabled} disabled={locked} onChange={enabled => onChange({ ...draft, enabled })} />
    </div>
    <div className={styles.fields}>
      <FormField label={t("可用模型")} hint={t("来自共享模型库")}>
        <ModelSelect mode="multiple" emptySummary={t("未选择")} label={t("选择 Codex 可用模型")} options={availability.options} value={draft.model_hashes} disabled={locked}
          onChange={model_hashes => onChange({ ...draft, model_hashes, default_model_hash: model_hashes.includes(draft.default_model_hash ?? "") ? draft.default_model_hash : model_hashes[0] ?? null })} />
        {availability.blockedPluginModels.length > 0 && <small className={styles.fieldNote}>
          {t("插件模型的 {count} 个模型不可用（Codex 只转发 API 模型）", { count: availability.blockedPluginModels.length })}
        </small>}
        {availability.missingSelections.length > 0 && <small className={styles.fieldNote}>
          {t("{count} 个已选模型不在模型库", { count: availability.missingSelections.length })}
        </small>}
        {availability.blockedPluginSelections.length > 0 && <small className={styles.fieldNote}>
          {t("请取消插件模型：Codex 无法使用它。")}
        </small>}
      </FormField>
      <FormField label={t("默认模型")}><ModelSelect mode="single" label={t("选择 Codex 默认模型")} options={selected} value={draft.default_model_hash ?? ""} disabled={locked || selected.length === 0} onChange={default_model_hash => onChange({ ...draft, default_model_hash })} /></FormField>
      <FormField label={t("Codex 配置目录")} hint={t("留空自动检测 .codex")}>
        <TextInput aria-label={t("Codex 配置目录")} disabled={locked} value={draft.home} placeholder={t("自动检测配置目录")} onChange={event => onChange({ ...draft, home: event.target.value })} />
      </FormField>
      <FormField label={t("Codex 网关端口")}><TextInput aria-label={t("Codex 网关端口")} disabled={locked} type="number" min={1} max={65535} value={draft.port} onChange={event => onChange({ ...draft, port: Number(event.target.value) })} /></FormField>
    </div>
  </div>;
}
