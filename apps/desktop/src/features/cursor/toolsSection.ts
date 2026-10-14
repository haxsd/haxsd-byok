/**
 * Cursor 页的两个分区。
 *
 * 「接入」讲怎么把 Cursor 接到本机代理，「功能设置」放 Cursor 自己功能要用的模型
 * 配置。分区写在 URL 上：设置页搜索「Tab 补全」「提交信息模型」时直接链到
 * `/harness/cursor?section=tools`，刷新或从书签打开也落在同一分区。
 */
export type CursorSettingsSection = "access" | "tools";

export const TOOLS_SECTION = "tools";

/** 只有明确写了 section=tools 才进功能设置；其余值（含旧链接）都回到接入。 */
export function cursorSettingsSection(params: URLSearchParams): CursorSettingsSection {
  return params.get("section") === TOOLS_SECTION ? "tools" : "access";
}
