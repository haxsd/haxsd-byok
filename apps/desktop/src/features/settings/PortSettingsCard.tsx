import { useState } from "react";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { TitledCard } from "../../shared/ui/TitledCard";
import styles from "./SettingsPage.module.scss";

export function PortSettingsCard() {
  const { ports } = useAppStore();
  const [draft, setDraft] = useState({ proxy_port: String(ports.proxy_port), service_port: String(ports.service_port) });
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const save = async () => {
    setError(null);
    const next = { proxy_port: Number(draft.proxy_port), service_port: Number(draft.service_port) };
    if (Object.values(draft).some(value => !value.trim()) || Object.values(next).some(value => !Number.isInteger(value) || value < 0 || value > 65535)) {
      setError(t("端口必须是 0–65535 之间的整数"));
      return;
    }
    setSaving(true);
    if (await appStore.updatePorts(next)) {
      setEditing(false);
      setStatus(t("已保存，重启软件后生效"));
    } else setError(appStore.getSnapshot().error ?? t("保存失败"));
    setSaving(false);
  };
  return <TitledCard title={t("端口设置")} action={editing ? <div className={styles.cardActions}>
    <Button size="small" disabled={saving} onClick={() => { setEditing(false); setError(null); }}>{t("取消")}</Button>
    <Button size="small" variant="primary" disabled={saving} onClick={() => void save()}>{saving ? t("保存中…") : t("保存")}</Button>
  </div> : <Button size="small" onClick={() => { setDraft({ proxy_port: String(ports.proxy_port), service_port: String(ports.service_port) }); setEditing(true); setStatus(null); }}>{t("编辑")}</Button>}>
    <div className={styles.portSettings}>
      <div className={styles.portFields}>
        {editing ? <>
          <FormField label={t("Cursor 代理端口")}><TextInput type="number" min={0} max={65535} value={draft.proxy_port} onChange={event => setDraft({ ...draft, proxy_port: event.target.value })} /></FormField>
          <FormField label={t("管理服务端口")}><TextInput type="number" min={0} max={65535} value={draft.service_port} onChange={event => setDraft({ ...draft, service_port: event.target.value })} /></FormField>
        </> : <>
          <div className={styles.portValue}><strong>{t("Cursor 代理端口")}</strong><span>{ports.proxy_port}</span></div>
          <div className={styles.portValue}><strong>{t("管理服务端口")}</strong><span>{ports.service_port}</span></div>
        </>}
      </div>
      <small>{t("0 表示启动时随机分配；端口被占用时会自动改用随机端口。修改后需重启软件。")}</small>
      {status && <small role="status">{status}</small>}
      {error && <small role="alert">{error}</small>}
    </div>
  </TitledCard>;
}
