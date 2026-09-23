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

export function applyTheme(themeId: ThemeId) {
  document.documentElement.dataset.theme = themeId;
}
