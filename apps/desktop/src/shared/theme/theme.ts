export const themeIds = ["midnight", "default-dark", "default-light"] as const;
export type ThemeId = (typeof themeIds)[number];

/** Shipped themes, in switcher order. */
export const themeOptions = [
  { id: "midnight" },
  { id: "default-dark" },
  { id: "default-light" },
] satisfies { id: ThemeId }[];

/**
 * The default look. This is the theme the product's own icon is drawn from, and the
 * one that makes it recognisably a different application from the one it was forked
 * from: the inherited theme is what the sibling product still ships.
 */
export const defaultThemeId: ThemeId = "midnight";

export function isThemeId(value: string | null): value is ThemeId {
  return value !== null && themeIds.some((id) => id === value);
}

const THEME_META: Record<ThemeId, { background: string; scheme: "dark" | "light" }> = {
  midnight: { background: "#0b0e14", scheme: "dark" },
  "default-dark": { background: "#141414", scheme: "dark" },
  "default-light": { background: "#f5f5f5", scheme: "light" },
};

export function applyTheme(themeId: ThemeId) {
  const meta = THEME_META[themeId];
  const root = document.documentElement;
  if (root.dataset) root.dataset.theme = themeId;
  if (root.style) root.style.colorScheme = meta.scheme;
  if (typeof document.querySelector === "function") {
    document.querySelector('meta[name="theme-color"]')?.setAttribute("content", meta.background);
  }
  try {
    if (typeof window !== "undefined") window.localStorage?.setItem("haxsd-byok.theme", themeId);
  } catch {
    // 隐私模式或无存储环境忽略防闪缓存写入。
  }
}
