import { useCallback, useEffect, useMemo, useState } from "react";
import { api, pluginText, type CommitSettingsView } from "../../shared/api";
import { commitPromptLocale, type CommitPromptLocale } from "../../i18n/runtime";
import { adoptInterfacePromptLocale, commitLocaleMismatch, loadCommitSettings } from "./commitSettings";
import { useI18n } from "../../i18n/store";
import { useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { Modal } from "../../shared/ui/Modal";
import { ModelSelect, type ModelSelectOption } from "../../shared/ui/ModelSelect";
import { claudeIcon, flatColorOrganizationIcon, openAiIcon } from "../../shared/ui/icons";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import controls from "../../shared/ui/Controls.module.scss";
import { modelProviderName } from "../../shared/utils/modelProvider";
import styles from "./CommitSettingsCard.module.scss";

/**
 * 提交信息的生成模型与提示词。
 *
 * 卡片永远展开，读模式显示已保存的模型，编辑只改本地草稿；读取失败在卡内说明，
 * 重试不必离开这一页。提示词编辑器仍然是独立弹窗：它要全高，不能挤在卡片里。
 */
export function CommitSettingsCard() {
  const { models, plugins } = useAppStore();
  const { locale } = useI18n();
  const message = useMessage();
  const [view, setView] = useState<CommitSettingsView | null>(null);
  const [modelDraft, setModelDraft] = useState("");
  const [editing, setEditing] = useState(false);
  const [savingModel, setSavingModel] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  /** 递增即重新读取；失败后的「重试」不改任何草稿，只是再问一次服务端。 */
  const [reloadToken, setReloadToken] = useState(0);
  const [promptOpen, setPromptOpen] = useState(false);
  const [promptDraft, setPromptDraft] = useState("");
  const [savingPrompt, setSavingPrompt] = useState(false);
  const [syncingPromptLocale, setSyncingPromptLocale] = useState(false);

  /**
   * 只读设置（`loadCommitSettings` 里没有任何写请求）。
   *
   * 这里以前会在 GET 之后补一次 PUT：没自定义过提示词、且服务端记的语言与当前界面语言不一致
   * 时，顺手把 prompt_locale 改成界面语言。后果是每次进这一页、每次切语言都会静默写一次库，
   * 用户根本没点过"保存"。现在只读，不一致时在卡片里提示，用户点了才写。
   */
  useEffect(() => {
    let active = true;
    void (async () => {
      try {
        const loaded = await loadCommitSettings(locale);
        if (active) {
          setView(loaded.view);
          setModelDraft(loaded.view.model_id);
          setLoadError(null);
        }
      } catch (cause) {
        if (active) setLoadError(errorText(cause));
      }
    })();
    return () => {
      active = false;
    };
  }, [locale, reloadToken]);

  // 默认提示词按服务端存的语言生成，界面语言决定编辑器里预览哪一份默认文案。
  const promptLocaleMismatch = view !== null && commitLocaleMismatch(view, locale);

  const adoptInterfaceLocale = useCallback(async () => {
    if (!view) return;
    setSyncingPromptLocale(true);
    try {
      const saved = await adoptInterfacePromptLocale(view, locale);
      setView(saved);
      setModelDraft(saved.model_id);
    } catch (cause) {
      message.error(cause);
    } finally {
      setSyncingPromptLocale(false);
    }
  }, [view, locale, message]);

  const modelOptions = useMemo(() => {
    const options: ModelSelectOption[] = [{ value: "", label: t("直连"), group: "Cursor" }];
    const seen = new Set<string>();
    for (const model of models) {
      seen.add(model.model_hash);
      options.push({
        value: model.model_hash,
        label: model.display_name && model.display_name !== model.model_id
          ? `${model.display_name}（${model.model_id}）`
          : model.display_name || model.model_id,
        group: modelProviderName(model),
        icon: model.type === "anthropic" ? claudeIcon : openAiIcon,
      });
    }
    for (const plugin of plugins) {
      for (const provider of plugin.providers) {
        if (!provider.configured) continue;
        const group = pluginText(provider.displayName, locale) || plugin.name;
        for (const model of provider.models.filter((model) => model.enabled)) {
          seen.add(model.id);
          options.push({
            value: model.id,
            label: model.displayName,
            group,
            iconSrc: model.icon || undefined,
            icon: model.icon ? undefined : flatColorOrganizationIcon,
          });
        }
      }
    }
    if (view?.model_id && !seen.has(view.model_id)) {
      options.push({ value: view.model_id, label: view.model_id, group: "Cursor" });
    }
    return options;
  }, [locale, models, plugins, view]);

  const persist = useCallback(
    async (modelId: string, prompt: string) => {
      if (!view) return null;
      const normalizedPrompt =
        prompt.trim() === view.default_prompt.trim() ? "" : prompt.trim();
      return api.setCommitSettings({
        model_id: modelId,
        prompt: normalizedPrompt,
        prompt_locale: commitPromptLocale(locale),
      });
    },
    [view, locale],
  );

  const editModel = useCallback(() => {
    if (!view) return;
    setModelDraft(view.model_id);
    setEditing(true);
  }, [view]);

  const cancelModelEdit = useCallback(() => {
    setModelDraft(view?.model_id ?? "");
    setEditing(false);
  }, [view]);

  const saveModel = useCallback(async () => {
    if (!view) return;
    setSavingModel(true);
    try {
      const saved = await persist(modelDraft, view.prompt);
      if (saved) {
        setView(saved);
        setModelDraft(saved.model_id);
        setEditing(false);
      }
    } catch (cause) {
      message.error(cause);
    } finally {
      setSavingModel(false);
    }
  }, [modelDraft, view, persist, message]);

  const openPrompt = useCallback(() => {
    if (!view) return;
    setPromptDraft(view.prompt || view.default_prompt);
    setPromptOpen(true);
  }, [view]);

  const savePrompt = useCallback(async () => {
    if (!view) return;
    setSavingPrompt(true);
    try {
      const saved = await persist(view.model_id, promptDraft);
      if (saved) setView(saved);
      setPromptOpen(false);
      message(t("提示词设置已保存"));
    } catch (cause) {
      message.error(cause);
    } finally {
      setSavingPrompt(false);
    }
  }, [view, promptDraft, persist, message]);

  const resetPrompt = useCallback(() => {
    if (!view) return;
    setPromptDraft(view.default_prompt);
  }, [view]);

  const selectedModelLabel = modelOptions.find((option) => option.value === view?.model_id)?.label
    ?? view?.model_id
    ?? (loadError ? t("读取失败") : t("加载中…"));
  const action = editing ? (
    <div className={styles.actionGroup}>
      <Button size="small" disabled={savingModel} onClick={cancelModelEdit}>{t("取消")}</Button>
      <Button variant="primary" size="small" disabled={savingModel} onClick={() => void saveModel()}>
        {savingModel ? t("保存中…") : t("保存")}
      </Button>
    </div>
  ) : (
    <div className={styles.actionGroup}>
      <button type="button" className={styles.textButton} disabled={!view} onClick={openPrompt}>
        {t("提示词设置")}
      </button>
      <button type="button" className={styles.textButton} disabled={!view} onClick={editModel}>
        {t("编辑")}
      </button>
    </div>
  );

  return (
    <>
      <TitledCard title={t("提交信息模型")} action={action}>
        <div className={styles.content}>
          <div className={styles.row}>
            <div className={styles.details}>
              <strong>{t("生成模型")}</strong>
            </div>
            {editing ? <div className={styles.select}>
              <ModelSelect
                mode="single"
                value={modelDraft}
                options={modelOptions}
                disabled={savingModel}
                label={t("生成模型")}
                onChange={setModelDraft}
              />
            </div> : <span className={styles.value}>{selectedModelLabel}</span>}
          </div>
          {loadError && <div className={styles.error} role="alert">
            <span>{t("读取提交信息设置失败：{error}", { error: loadError })}</span>
            <Button size="small" onClick={() => setReloadToken((current) => current + 1)}>{t("重试")}</Button>
          </div>}
          {promptLocaleMismatch && <div className={styles.notice} role="status">
            <span>{t("默认提示词当前用的是{current}，与界面语言（{wanted}）不一致：改过来才会按当前语言生成提交信息。", {
              current: promptLocaleLabel(view.prompt_locale),
              wanted: promptLocaleLabel(commitPromptLocale(locale)),
            })}</span>
            <Button size="small" disabled={syncingPromptLocale} onClick={() => void adoptInterfaceLocale()}>
              {syncingPromptLocale ? t("保存中…") : t("使用界面语言的默认提示词")}
            </Button>
          </div>}
        </div>
      </TitledCard>

      <Modal
        open={promptOpen}
        title={t("提示词设置")}
        wide
        fullHeight
        busy={savingPrompt}
        onClose={() => setPromptOpen(false)}
        onSubmit={() => void savePrompt()}
        submitLabel={t("保存")}
        closeLabel={t("取消")}
        secondaryAction={
          <button type="button" className={controls.secondary} onClick={resetPrompt}>
            {t("恢复默认")}
          </button>
        }
      >
        <textarea
          className={styles.promptEditor}
          value={promptDraft}
          spellCheck={false}
          aria-label={t("提交信息提示词")}
          onChange={(event) => setPromptDraft(event.target.value)}
        />
      </Modal>
    </>
  );
}

function errorText(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}

/** 语言标签直接写中文/英文名，和设置页的语言下拉一致，不走 t()。 */
function promptLocaleLabel(locale: CommitPromptLocale) {
  return locale === "zh-CN" ? "简体中文" : "English";
}
