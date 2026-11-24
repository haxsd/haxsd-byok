import { useEffect, useState } from "react";
import { api, type ExternalApiSettings } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import styles from "./ExternalApiSettingsCard.module.scss";

function newKey(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(32));
  return [...bytes].map(value => value.toString(16).padStart(2, "0")).join("");
}

export function ExternalApiSettingsCard({ visible }: { visible: boolean }) {
  const message = useMessage();
  const [saved, setSaved] = useState<ExternalApiSettings | null>(null);
  const [draft, setDraft] = useState<ExternalApiSettings>({ enabled: false, api_key: "" });
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const endpoint = `${window.location.origin}/byok/v1`;

  useEffect(() => {
    if (!visible) return;
    let live = true;
    api.externalApiSettings()
      .then(value => {
        if (!live) return;
        setSaved(value);
        setDraft(value);
      })
      .catch(cause => {
        if (live) message.error(cause);
      });
    return () => {
      live = false;
    };
  }, [visible, message]);

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
            <TextInput value={draft.api_key} onChange={event => setDraft({ ...draft, api_key: event.target.value })} />
            <Button size="small" disabled={saving} onClick={() => setDraft({ ...draft, api_key: newKey() })}>{t("生成密钥")}</Button>
          </div>
        </FormField>
      </> : saved ? <>
        <div className={styles.row}><strong>{t("状态")}</strong><span>{saved.enabled ? t("已开启") : t("已关闭")}</span></div>
        <div className={styles.row}><strong>{t("接口地址")}</strong><code>{endpoint}</code></div>
        {saved.enabled && <small className={styles.hint}>{t("请求时使用上方地址；模型列表请先通过 /models 获取。")}</small>}
      </> : <small>{t("正在读取外部 API 设置…")}</small>}
    </div>
  </TitledCard>;
}
