import { useEffect, useMemo, useRef, useState } from "react";
import { api, configuredPluginModels, type ExternalApiSettings } from "../../shared/api";
import { useAppStoreSelector } from "../../shared/store/appStore";
import { selectModels, selectPlugins } from "../../shared/store/selectors";
import { Button } from "../../shared/ui/Button";
import { FormField, SecretTextInput } from "../../shared/ui/FormControls";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import { modelProviderName } from "../../shared/utils/modelProvider";
import styles from "./ExternalApiSettingsCard.module.scss";

function newKey(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(32));
  return [...bytes].map(value => value.toString(16).padStart(2, "0")).join("");
}

function maskKey(key: string): string {
  if (!key) return "-";
  if (key.length <= 12) return "••••••••";
  return `${key.slice(0, 6)}••••${key.slice(-4)}`;
}

export function ExternalApiSettingsCard({ visible }: { visible: boolean }) {
  const models = useAppStoreSelector(selectModels);
  const plugins = useAppStoreSelector(selectPlugins);
  const message = useMessage();
  const [saved, setSaved] = useState<ExternalApiSettings | null>(null);
  const [draft, setDraft] = useState<ExternalApiSettings>({ enabled: false, api_key: "" });
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [loadError, setLoadError] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);
  const [showKey, setShowKey] = useState(false);
  const editingRef = useRef(false);
  editingRef.current = editing;
  const openaiBaseUrl = `${window.location.origin}/byok/v1`;
  const anthropicBaseUrl = `${window.location.origin}/byok`;

  const availableModelIds = useMemo(() => {
    const builtIn = models.map(model => `${modelProviderName(model)}/${model.model_id}`);
    const plugin = configuredPluginModels(plugins).map(
      model => `plugin:${model.pluginId}:${model.providerId}/${model.modelId}`,
    );
    return [...new Set([...builtIn, ...plugin])].slice(0, 8);
  }, [models, plugins]);

  useEffect(() => {
    if (!visible) return;
    const controller = new AbortController();
    setLoadError(false);
    api.externalApiSettings(controller.signal)
      .then(value => {
        if (controller.signal.aborted) return;
        setSaved(value);
        if (!editingRef.current) setDraft(value);
      })
      .catch(cause => {
        if (controller.signal.aborted) return;
        setLoadError(true);
        message.error(cause);
      });
    return () => controller.abort();
  }, [visible, message, reloadKey]);

  const copy = (value: string, successText: string) => {
    void api.copyCursorText(value)
      .then(() => message(successText))
      .catch(cause => message.error(cause));
  };

  const edit = () => {
    if (!saved) return;
    setDraft(saved);
    setEditing(true);
  };
  const save = async () => {
    if (draft.enabled && !draft.api_key.trim()) {
      message.error(t("启用外部 API 前请先生成或填写密钥"));
      return;
    }
    setSaving(true);
    try {
      const value = await api.setExternalApiSettings({
        enabled: draft.enabled,
        api_key: draft.api_key.trim(),
      });
      setSaved(value);
      setDraft(value);
      setEditing(false);
      message(t("外部 API 设置已保存"));
    } catch (cause) {
      message.error(cause);
    } finally {
      setSaving(false);
    }
  };
  const actions = editing ? <div className={styles.actions}>
    <Button size="small" disabled={saving} onClick={() => setEditing(false)}>{t("取消")}</Button>
    <Button size="small" variant="primary" disabled={saving} onClick={() => void save()}>{saving ? t("保存中…") : t("保存")}</Button>
  </div> : <Button size="small" disabled={!saved} onClick={edit}>{t("编辑")}</Button>;

  const sampleModel = availableModelIds[0] ?? "<group>/<model_id>";
  const sampleCurl = `curl ${openaiBaseUrl}/chat/completions -H "Authorization: Bearer ${saved?.api_key || "<YOUR_KEY>"}" -H "Content-Type: application/json" -d '{"model":"${sampleModel}","messages":[{"role":"user","content":"Hi"}]}'`;

  return <TitledCard title={t("外部 API")} action={actions}>
    <div className={styles.content}>
      <p className={styles.description}>{t("让脚本或其他客户端通过 OpenAI / Anthropic 兼容接口使用同一套模型库。只监听本机，默认关闭。")}</p>
      {editing ? <>
        <div className={styles.row}>
          <div><strong>{t("开启外部 API")}</strong><small>{t("关闭后，所有请求都会返回 403。")}</small></div>
          <Switch checked={draft.enabled} disabled={saving} label={t("开启外部 API")} onChange={enabled => setDraft({ ...draft, enabled })} />
        </div>
        <FormField label={t("API 密钥")} hint={t("请求必须带 Authorization: Bearer <密钥> 或 x-api-key 头。")}>
          <div className={styles.keyRow}>
            <SecretTextInput value={draft.api_key} onChange={event => setDraft({ ...draft, api_key: event.target.value })} />
            <Button size="small" disabled={saving} onClick={() => setDraft({ ...draft, api_key: newKey() })}>{t("生成密钥")}</Button>
          </div>
        </FormField>
      </> : loadError && !saved ? <div className={styles.row}>
        <small>{t("读取外部 API 设置失败")}</small>
        <Button size="small" onClick={() => setReloadKey(key => key + 1)}>{t("重试")}</Button>
      </div> : saved ? <>
        <div className={styles.row}><strong>{t("状态")}</strong><span>{saved.enabled ? t("已开启") : t("已关闭")}</span></div>
        <div className={styles.row}>
          <strong>{t("OpenAI 接口地址")}</strong>
          <div className={styles.inlineValue}>
            <code>{openaiBaseUrl}</code>
            <Button size="small" onClick={() => copy(openaiBaseUrl, t("OpenAI 接口地址已复制"))}>{t("复制")}</Button>
          </div>
        </div>
        <div className={styles.row}>
          <strong>{t("Anthropic 接口地址")}</strong>
          <div className={styles.inlineValue}>
            <code>{anthropicBaseUrl}</code>
            <Button size="small" onClick={() => copy(anthropicBaseUrl, t("Anthropic 接口地址已复制"))}>{t("复制")}</Button>
          </div>
        </div>
        {saved.api_key && <div className={styles.row}>
          <strong>{t("API 密钥")}</strong>
          <div className={styles.inlineValue}>
            <code>{showKey ? saved.api_key : maskKey(saved.api_key)}</code>
            <Button size="small" onClick={() => setShowKey(current => !current)}>{showKey ? t("隐藏") : t("显示")}</Button>
            <Button size="small" onClick={() => copy(saved.api_key, t("API 密钥已复制"))}>{t("复制")}</Button>
          </div>
        </div>}
        {saved.enabled && <details className={styles.examples}>
          <summary>{t("可用模型 ID 与测试示例")}</summary>
          <div className={styles.exampleBody}>
            {availableModelIds.length > 0 && <div className={styles.modelChips}>
              {availableModelIds.map(id => (
                <button key={id} type="button" className={styles.modelChip} onClick={() => copy(id, t("模型 ID 已复制"))}>
                  <code>{id}</code>
                </button>
              ))}
            </div>}
            <Button size="small" onClick={() => copy(sampleCurl, t("curl 测试命令已复制"))}>{t("复制 curl 示例")}</Button>
          </div>
        </details>}
      </> : <small>{t("正在读取外部 API 设置…")}</small>}
    </div>
  </TitledCard>;
}
