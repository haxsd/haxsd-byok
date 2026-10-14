export type SettingsSectionId = "general" | "network" | "usage" | "advanced" | "about";
export type SettingsItemId = "appearance" | "app" | "proxy" | "recording" | "pricing" | "storage" | "developer" | "ports" | "diagnostics" | "update";
export type SettingsSection = {
  id: SettingsSectionId;
  label: string;
  items: Array<{ id: SettingsItemId; label: string; keywords: string }>;
};

/** Build during render so labels follow the selected interface language. */
export function settingsSections(developerMode: boolean): SettingsSection[] {
  return [
    { id: "general", label: t("常规"), items: [
      { id: "appearance", label: t("外观与语言"), keywords: "语言 主题 外观 language theme appearance" },
      { id: "app", label: t("启动与窗口"), keywords: "开机 静默 启动 窗口 托盘 dock autostart startup window" },
    ] },
    { id: "network", label: t("网络"), items: [
      { id: "proxy", label: t("出网代理"), keywords: "网络 代理 proxy network address auth 认证" },
    ] },
    { id: "usage", label: t("记录与用量"), items: [
      { id: "recording", label: t("调用记录"), keywords: "详细 请求 响应 隐私 detailed recording request response" },
      { id: "pricing", label: t("模型定价"), keywords: "价格 定价 费用 币种 高峰 低谷 token pricing cost currency" },
      { id: "storage", label: t("存储管理"), keywords: "存储 空间 清理 删除 磁盘 storage cleanup disk" },
    ] },
    { id: "advanced", label: t("高级与诊断"), items: [
      { id: "developer", label: t("开发者模式"), keywords: "开发 调试 端口 developer debug port" },
      ...(developerMode ? [{ id: "ports" as const, label: t("端口设置"), keywords: "端口 监听 服务 port listen server" }] : []),
      { id: "diagnostics", label: t("诊断"), keywords: "日志 版本 路径 诊断 log version path diagnostics" },
    ] },
    { id: "about", label: t("关于与更新"), items: [
      { id: "update", label: t("软件更新"), keywords: "更新 版本 安装 下载 update version install download" },
    ] },
  ];
}

export function settingsDestinations(query: string) {
  const words = query.trim().toLocaleLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return [];
  return [
    { label: t("Tab 补全"), keywords: "tab 补全 autocomplete", path: "/harness/cursor?section=tools" },
    { label: t("提交信息模型"), keywords: "commit 提交 prompt 提示词", path: "/harness/cursor?section=tools" },
    { label: t("导入模型"), keywords: "import 导入 模型", path: "/models" },
  ].filter(item => matchesWords(`${item.label} ${item.keywords}`, words));
}

function matchesWords(text: string, words: string[]) {
  const normalized = text.toLocaleLowerCase();
  return words.every(word => normalized.includes(word));
}

/** Category switches replace content; a search filters individual settings across categories. */
export function visibleSettingsSections(sections: SettingsSection[], selected: string | null, query: string): SettingsSection[] {
  const words = query.trim().toLocaleLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return [sections.find(section => section.id === selected) ?? sections[0]];
  return sections.map(section => ({
    ...section,
    items: section.items.filter(item => matchesWords(`${section.label} ${item.label} ${item.keywords}`, words)),
  })).filter(section => section.items.length > 0);
}
