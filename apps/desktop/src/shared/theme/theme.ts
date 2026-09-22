export const themeIds = ["midnight", "default-dark", "default-light"] as const;
export type ThemeId = (typeof themeIds)[number];

/** Shipped themes, in switcher order. */
export const themeOptions = [
  { id: "midnight" },
  { id: "default-dark" },
  { id: "default-light" },
] satisfies { id: ThemeId }[];

/** Kept as the original theme: switching a product's default look is the owner's call. */
export const defaultThemeId: ThemeId = "default-dark";

export function isThemeId(value: string | null): value is ThemeId {
  return value !== null && themeIds.some((id) => id === value);
}

export function applyTheme(themeId: ThemeId) {
  document.documentElement.dataset.theme = themeId;
}
