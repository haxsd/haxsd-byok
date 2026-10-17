import type { IconifyIcon } from "@iconify/react";
import { harnessClients } from "../features/harness/harnessClients";
import type { LlmCall, Model } from "../shared/api";
import { appStore } from "../shared/store/appStore";
import { themeOptions, type ThemeId } from "../shared/theme/theme";
import { callStatusLabel } from "../shared/utils/callStatus";
import { activityIcon, eyeIcon, refreshIcon } from "../shared/ui/icons";
import { navCallsIcon, navHarnessIcon, navModelsIcon, navOverviewIcon, navPluginsIcon, navSettingsIcon, navTutorialIcon } from "../shared/ui/navIcons";

export type PaletteCommand = {
  id: string;
  group: string;
  label: string;
  hint?: string;
  icon: IconifyIcon | string;
  keywords?: string;
  run: () => void;
};

/**
 * 命令面板的命令表。
 *
 * 从组件里抽出来只为一件事：这些 `run` 是**动作**，而动作失败必须有人接。打开调用详情
 * 这条曾经直接 `void api.openCallDetails(...)`，服务不可达时 rejection 没人接、面板照常
 * 关闭，界面一点反馈都没有；同一动作在调用表和首页走的是 `appStore`，错误会变成提示条。
 * 抽成模块后可以用假服务端断言「点了之后 store 里有没有错误」，组件只负责画。
 */
export function commandPaletteCommands(input: {
  calls: LlmCall[];
  models: Model[];
  locale: string;
  theme: ThemeId;
  navigate: (path: string) => void;
  close: () => void;
}): PaletteCommand[] {
  const { calls, models, locale, theme, navigate, close } = input;
  const go = (path: string) => () => { navigate(path); close(); };
  const pages: PaletteCommand[] = [
    { id: "page-overview", group: t("页面"), label: t("概览"), icon: navOverviewIcon, run: go("/") },
    { id: "page-calls", group: t("页面"), label: t("调用"), icon: navCallsIcon, run: go("/calls") },
    // 接入页里可以切换客户端，但常用客户端留在命令面板里直达。
    { id: "page-harness", group: t("页面"), label: t("接入"), icon: navHarnessIcon, run: go("/harness/cursor") },
    { id: "page-models", group: t("页面"), label: t("模型库"), icon: navModelsIcon, run: go("/models") },
    { id: "page-plugins", group: t("页面"), label: t("插件配置"), icon: navPluginsIcon, run: go("/plugins") },
    { id: "page-settings", group: t("页面"), label: t("系统设置"), icon: navSettingsIcon, run: go("/settings") },
    { id: "page-tutorial", group: t("页面"), label: t("使用教程"), icon: navTutorialIcon, run: go("/tutorial") },
  ];
  // 「页面」一段必须连着排：分组标题按相邻项合并，中间插进别的组会把「页面」印两次。
  const clients: PaletteCommand[] = harnessClients.map((client) => ({
    id: `page-harness-${client.id}`,
    group: t("接入模块"),
    label: client.label,
    icon: client.icon,
    run: go(client.path),
  }));
  const actions: PaletteCommand[] = [
    {
      id: "action-refresh",
      group: t("动作"),
      label: t("刷新数据"),
      hint: t("重新读取模型、调用与状态"),
      icon: refreshIcon,
      run: () => { void appStore.refresh(); close(); },
    },
    ...themeOptions.map((option) => ({
      id: `theme-${option.id}`,
      group: t("动作"),
      label: t("切换到{name}", { name: themeName(option.id) }),
      hint: option.id === theme ? t("当前主题") : undefined,
      icon: navSettingsIcon,
      keywords: option.id,
      run: () => { appStore.selectTheme(option.id); close(); },
    })),
    {
      id: "action-tutorial",
      group: t("动作"),
      // 教程是这个应用自己的一页，不是上游产品的文档站：打开外部网页会把用户带到
      // 另一套界面和步骤上去。
      label: t("打开使用教程"),
      icon: navTutorialIcon,
      run: go("/tutorial"),
    },
  ];
  const modelCommands: PaletteCommand[] = models.slice(0, 8).map((model) => ({
    id: `model-${model.model_hash}`,
    group: t("模型"),
    // 缺字段时逐级回退：标签总要有个能读的东西，也避免 undefined 进入 matchScore。
    label: model.display_name || model.model_id || model.model_hash,
    hint: model.model_id,
    icon: navModelsIcon,
    keywords: `${model.model_id ?? ""} ${model.base_url ?? ""}`,
    run: () => { navigate("/models"); close(); },
  }));
  const callCommands: PaletteCommand[] = calls.slice(0, 40).map((call) => ({
    id: `call-${call.call_id}`,
    group: t("调用"),
    label: call.display_name || call.model_id || call.call_id,
    hint: `${new Date(call.created_at_ms).toLocaleString(locale)} · ${callStatusLabel(call.status)}`,
    icon: call.status === "completed" ? activityIcon : eyeIcon,
    keywords: `${call.call_id} ${call.conversation_id ?? ""} ${call.model_id ?? ""}`,
    // 和调用表、首页最近调用共用 store 的错误通道：服务不可达时这条命令原来会静默
    // 什么都不做（裸 api 的 rejection 没人接），现在至少会弹出一条提示。
    run: () => { void appStore.openCallDetails(call.call_id); close(); },
  }));
  return [...pages, ...clients, ...actions, ...modelCommands, ...callCommands];
}

function themeName(id: string) {
  if (id === "midnight") return t("午夜靛蓝");
  if (id === "default-dark") return t("默认暗色");
  return t("默认亮色");
}
