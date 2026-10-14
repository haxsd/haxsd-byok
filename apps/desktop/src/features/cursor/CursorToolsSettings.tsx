import { useCallback, useEffect, useState } from "react";
import { api, type TabSettings } from "../../shared/api";
import { CommitSettingsCard } from "./CommitSettingsCard";
import { TabSettingsCard } from "./TabSettingsCard";
import { useMessage } from "../../shared/ui/message";

/** 自定义模式的地址是必填的：TAB 接口路径会追加到它后面。保存前统一去掉首尾空格。 */
export function normalizeTabDraft(draft: TabSettings): TabSettings {
  if (draft.mode === "custom" && !draft.address.trim()) throw new Error(t("TAB 服务地址不能为空"));
  return { mode: draft.mode, address: draft.address.trim() };
}

/**
 * Cursor 功能设置的容器：负责 TAB 补全配置的 HTTP 读取、编辑与保存。
 *
 * 提交信息模型由它下面的卡片自己读写——两者是彼此独立的接口，凑在一起只因为这
 * 两件事都属于「Cursor 功能设置」。容器不折叠任何卡片：编辑时表单必须直接可见。
 */
export function CursorToolsSettings() {
  const message = useMessage();
  const [settings, setSettings] = useState<TabSettings | null>(null);
  const [draft, setDraft] = useState<TabSettings>({ mode: "public", address: "" });
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  /** 读取失败的原因：行内展示，重试就地再读一次。 */
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const loaded = await api.tabSettings();
      setSettings(loaded);
      setDraft(loaded);
      setError(null);
    } catch (cause) {
      setError(errorText(cause));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const edit = () => {
    if (!settings) return;
    setDraft(settings);
    setEditing(true);
  };
  const cancel = () => {
    if (settings) setDraft(settings);
    setEditing(false);
  };
  const save = async () => {
    try {
      const next = normalizeTabDraft(draft);
      setSaving(true);
      const saved = await api.setTabSettings(next);
      setSettings(saved);
      setDraft(saved);
      setEditing(false);
      message(t("TAB 设置已保存"));
    } catch (cause) {
      message.error(cause);
    } finally {
      setSaving(false);
    }
  };

  return <>
    <TabSettingsCard
      settings={settings}
      draft={draft}
      editing={editing}
      saving={saving}
      error={error}
      onDraftChange={setDraft}
      onEdit={edit}
      onCancel={cancel}
      onSave={() => void save()}
      onRetry={() => void load()}
    />
    <CommitSettingsCard />
  </>;
}

function errorText(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}
