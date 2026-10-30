import { useEffect, useState } from "react";
import { api, type OutboundStatus, type ProxySettings, type ProxySettingsInput } from "../../shared/api";
import { useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { useMessage } from "../../shared/ui/message";
import { loadWhenVisible } from "../../shared/utils/visibleLoad";
import { ProxySettingsCard } from "./ProxySettingsCard";
import { loadNetworkSettings } from "./settingsLoaders";
import styles from "./SettingsPage.module.scss";

/**
 * `visible` 来自设置页：分区/搜索没显示这张卡时不要取数。
 *
 * 卡片仍然挂在树上（`hidden` 只隐藏，草稿和编辑态留在本地），但请求要等它真的被看到——
 * 进设置页就为五个分区里所有卡片各发一轮请求，是纯粹的浪费。门在 `loadWhenVisible` 里，
 * 组件不再自己写 `if (!visible) return`：判据只有一处，才钉得住。
 */
export function NetworkSettingsCard({ visible }: { visible: boolean }) {
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
    void loadWhenVisible(visible, loadNetworkSettings).then(loaded => {
      if (!active || !loaded) return;
      if (loaded.settings) setSettings(loaded.settings);
      if (loaded.settingsError) setError(loaded.settingsError);
      if (loaded.outbound) setOutbound(loaded.outbound);
      if (loaded.outboundError) setStatusError(loaded.outboundError);
    });
    return () => { active = false; };
  }, [attempt, visible]);
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
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setSaving(false); }
  };
  return <div>
    <ProxySettingsCard settings={settings} outbound={outbound} cursorMode={cursorHarness?.mode ?? "official"} draft={draft} editing={editing} saving={saving} error={error} onDraftChange={setDraft} onEdit={edit} onCancel={cancel} onSave={() => void save()} />
    {(error || statusError) && <div className={styles.inlineError} role="alert"><span>{error || statusError}</span>{!editing && <Button size="small" onClick={() => setAttempt(value => value + 1)}>{t("重试")}</Button>}</div>}
  </div>;
}
