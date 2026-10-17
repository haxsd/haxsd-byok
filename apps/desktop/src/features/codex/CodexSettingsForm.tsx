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
    {/* 开关带一行自己的说明：「撤销接管后才能修改设置」是这一行控件被锁住的原因，
        不是压在表单上面的一句孤立提示。 */}
    <div className={styles.settingRow}>
      <div>
        <strong>{t("启用 Codex 网关")}</strong>
        <small>{locked ? t("撤销接管后才能修改设置") : t("关闭时不会监听端口，Codex 也不会指向本机网关。")}</small>
      </div>
      <Switch label={t("启用 Codex 网关")} checked={draft.enabled} disabled={locked} onChange={enabled => onChange({ ...draft, enabled })} />
    </div>
    <div className={styles.fields}>
      <FormField label={t("可用模型")} hint={t("共享模型库中的 API 模型。请先为每个模型填写真实上下文窗口。")}>
        <ModelSelect mode="multiple" label={t("选择 Codex 可用模型")} options={availability.options} value={draft.model_hashes} disabled={locked}
          onChange={model_hashes => onChange({ ...draft, model_hashes, default_model_hash: model_hashes.includes(draft.default_model_hash ?? "") ? draft.default_model_hash : model_hashes[0] ?? null })} />
        {/* 「选不到」必须有个说法：模型库页把插件模型和 API 模型并排列出来，这里是唯一
            能解释为什么插件模型不在此处的原因。 */}
        {availability.blockedPluginModels.length > 0 && <small className={styles.fieldNote}>
          {t("插件提供的 {count} 个模型不能用于 Codex：本机网关只转发模型库里的 API 模型。", { count: availability.blockedPluginModels.length })}
          {localizedNames(availability.blockedPluginModels)}
        </small>}
        {availability.missingSelections.length > 0 && <small className={styles.fieldNote}>
          {t("已选的 {count} 个模型已不在模型库里；取消它们或先在模型库补回来。", { count: availability.missingSelections.length })}
        </small>}
        {availability.blockedPluginSelections.length > 0 && <small className={styles.fieldNote}>
          {t("插件模型的设置项在 Codex 网关里无法生效，请取消它们。")}
        </small>}
      </FormField>
      <FormField label={t("默认模型")}><ModelSelect mode="single" label={t("选择 Codex 默认模型")} options={selected} value={draft.default_model_hash ?? ""} disabled={locked || selected.length === 0} onChange={default_model_hash => onChange({ ...draft, default_model_hash })} /></FormField>
      <FormField label={t("Codex 配置目录")} hint={t("留空使用 CODEX_HOME 或用户目录下的 .codex。只在点击应用时修改 config.toml。")}>
        <TextInput aria-label={t("Codex 配置目录")} disabled={locked} value={draft.home} placeholder={t("自动检测配置目录")} onChange={event => onChange({ ...draft, home: event.target.value })} />
      </FormField>
      <FormField label={t("Codex 网关端口")}><TextInput aria-label={t("Codex 网关端口")} disabled={locked} type="number" min={1} max={65535} value={draft.port} onChange={event => onChange({ ...draft, port: Number(event.target.value) })} /></FormField>
    </div>
  </div>;
}

/** 插件模型名跟在说明后面，用户才知道「那 1 个」是哪个。 */
function localizedNames(models: PluginModelDescriptor[]) {
  const names = models.slice(0, 3).map((model) => model.displayName).join("、");
  return names ? `（${names}${models.length > 3 ? "…" : ""}）` : "";
}
