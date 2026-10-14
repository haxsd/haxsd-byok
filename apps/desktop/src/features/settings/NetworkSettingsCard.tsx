import { useEffect, useState } from "react";
import { api, type OutboundStatus, type ProxySettings, type ProxySettingsInput } from "../../shared/api";
import { useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { useMessage } from "../../shared/ui/message";
import { ProxySettingsCard } from "./ProxySettingsCard";
import styles from "./SettingsPage.module.scss";

export function NetworkSettingsCard() {
  const { cursorHarness } = useAppStore();
  const message = useMessage();
  const [settings, setSettings] = useState<ProxySettings | null>(null);
  const [outbound, setOutbound] = useState<OutboundStatus | null>(null);
  const [draft, setDraft] = useState<ProxySettingsInput>({ mode: "default", address: "", auth_enabled: false, username: "", password: "" });
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let active = true;
    setError(null);
    setStatusError(null);
    void api.proxySettings().then(value => { if (active) setSettings(value); }).catch(cause => { if (active) setError(String(cause)); });
    void api.outboundStatus().then(value => { if (active) setOutbound(value); }).catch(cause => { if (active) setStatusError(String(cause)); });
    return () => { active = false; };
  }, [attempt]);
  const cancel = () => { setEditing(false); setError(null); };
  const edit = () => {
    if (!settings) return;
    setDraft({ mode: settings.mode, address: settings.address, auth_enabled: settings.auth_enabled, username: settings.username, password: "" });
    setError(null);
    setEditing(true);
  };
  const save = async () => {
    setSaving(true);
    setError(null);
    try {
      setSettings(await api.setProxySettings({ ...draft, password: draft.password || undefined }));
      setEditing(false);
      message(t("代理设置已保存"));
    } catch (cause) { setError(String(cause)); }
    finally { setSaving(false); }
  };
  return <div>
    <ProxySettingsCard settings={settings} outbound={outbound} cursorMode={cursorHarness?.mode ?? "official"} draft={draft} editing={editing} saving={saving} error={error} onDraftChange={setDraft} onEdit={edit} onCancel={cancel} onSave={() => void save()} />
    {(error || statusError) && <div className={styles.inlineError} role="alert"><span>{error || statusError}</span>{!editing && <Button size="small" onClick={() => setAttempt(value => value + 1)}>{t("重试")}</Button>}</div>}
  </div>;
}
