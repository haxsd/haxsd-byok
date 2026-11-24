import { useEffect, useMemo, useState } from "react";
import { api, type VisionSettings } from "../../shared/api";
import { useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { ModelSelect, type ModelSelectOption } from "../../shared/ui/ModelSelect";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import { modelProviderName } from "../../shared/utils/modelProvider";
import styles from "./VisionSettingsCard.module.scss";

export function VisionSettingsCard({ visible }: { visible: boolean }) {
  const { models } = useAppStore();
  const message = useMessage();
  const [saved, setSaved] = useState<VisionSettings | null>(null);
  const [enabled, setEnabled] = useState(false);
  const [modelHash, setModelHash] = useState("");
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const options = useMemo<ModelSelectOption[]>(
    () => models
      .filter(model => model.supports_images)
      .map(model => ({
        value: model.model_hash,
        label: model.display_name || model.model_id,
        group: modelProviderName(model),
      })),
    [models],
  );

  useEffect(() => {
    if (!visible) return;
    let live = true;
    api.visionSettings()
      .then(value => {
        if (!live) return;
        setSaved(value);
        setEnabled(value.mode === "model");
        setModelHash(value.mode === "model" ? value.model_hash : "");
      })
      .catch(cause => {
        if (live) message.error(cause);
      });
    return () => {
      live = false;
    };
  }, [visible, message]);

  const edit = () => {
    setEnabled(saved?.mode === "model");
    setModelHash(saved?.mode === "model" ? saved.model_hash : "");
    setEditing(true);
  };
  const save = async () => {
    if (enabled && !modelHash) {
      message.error(t("请选择能读取图片的模型"));
      return;
    }
    setSaving(true);
    try {
      const value = await api.setVisionSettings(
        enabled ? { mode: "model", model_hash: modelHash } : { mode: "disabled" },
      );
      setSaved(value);
      setEditing(false);
      message(t("图片识别设置已保存"));
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

  return <TitledCard title={t("图片识别")} action={actions}>
    <div className={styles.content}>
      <p className={styles.description}>{t("当对话模型不能读取图片时，先用另一个视觉模型把图片描述成文字。只在必要时调用。")}</p>
      {editing ? <>
        <div className={styles.row}>
          <div><strong>{t("启用图片描述兜底")}</strong><small>{t("关闭时，不支持图片的模型仍会直接收到图片并可能报错。")}</small></div>
          <Switch checked={enabled} disabled={saving} label={t("启用图片描述兜底")} onChange={setEnabled} />
        </div>
        {enabled && (options.length > 0
          ? <ModelSelect mode="single" label={t("视觉模型")} options={options} value={modelHash} disabled={saving} onChange={setModelHash} emptySummary={t("选择视觉模型")} />
          : <small>{t("模型库里还没有标记为支持图片的模型，请先添加或在模型设置里启用图片输入。")}</small>)}
      </> : saved?.mode === "model"
        ? <div className={styles.row}><strong>{t("视觉模型")}</strong><span>{models.find(model => model.model_hash === saved.model_hash)?.display_name ?? saved.model_hash}</span></div>
        : <small>{t("未启用。纯文本模型收到图片时仍会由上游决定是否报错。")}</small>}
    </div>
  </TitledCard>;
}
