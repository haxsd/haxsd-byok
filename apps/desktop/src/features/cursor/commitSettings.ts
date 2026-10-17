import { api, type CommitSettingsView } from "../../shared/api";
import { commitPromptLocale, type Locale } from "../../i18n/runtime";

/**
 * 默认提示词的语言与界面语言不一致。
 *
 * 不一致本身不是错误：默认提示词按服务端记的 `prompt_locale` 生成，编辑器里预览的是当前
 * 界面语言的默认文案。界面要把这件事说出来，让用户自己决定换不换。
 */
export function commitLocaleMismatch(view: CommitSettingsView, locale: Locale): boolean {
  return view.prompt.trim() === "" && view.prompt_locale !== commitPromptLocale(locale);
}

/**
 * 读提交信息设置：**只读**。
 *
 * 这里以前在 GET 之后顺手 PUT 一次（`prompt` 为空且 `prompt_locale` 与界面语言不一致时，
 * 把语言改成界面语言）：于是每次进这一页、每次切语言都会静默写一次库，用户根本没点过保存。
 * 现在只报告不一致，写是显式动作（见 `adoptInterfacePromptLocale`）。
 */
export async function loadCommitSettings(locale: Locale): Promise<{ view: CommitSettingsView; localeMismatch: boolean }> {
  const view = await api.commitSettings(locale);
  return { view, localeMismatch: commitLocaleMismatch(view, locale) };
}

/** 显式动作：把默认提示词切到界面语言。只有用户点了才调用。 */
export function adoptInterfacePromptLocale(view: CommitSettingsView, locale: Locale): Promise<CommitSettingsView> {
  return api.setCommitSettings({
    model_id: view.model_id,
    prompt: "",
    prompt_locale: commitPromptLocale(locale),
  });
}
