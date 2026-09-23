import type { Locale } from "../../i18n/runtime";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/**
 * "3 分钟前" / "3 minutes ago".
 *
 * A call list is read as a timeline, and an absolute timestamp makes the reader do
 * the subtraction. Past a week the absolute date is easier to place, so it takes over
 * instead of turning into "23 天前".
 */
export function formatRelativeTime(epochMs: number, locale: Locale, now = Date.now()) {
  const formatter = new Intl.RelativeTimeFormat(locale, { numeric: "auto" });
  const delta = epochMs - now;
  const magnitude = Math.abs(delta);
  if (magnitude < MINUTE) return formatter.format(0, "second");
  if (magnitude < HOUR) return formatter.format(Math.round(delta / MINUTE), "minute");
  if (magnitude < DAY) return formatter.format(Math.round(delta / HOUR), "hour");
  if (magnitude < 7 * DAY) return formatter.format(Math.round(delta / DAY), "day");
  return new Intl.DateTimeFormat(locale, { dateStyle: "medium" }).format(new Date(epochMs));
}

/** 时间跨度，用于耗时一类的短区间。 */
export function formatDuration(milliseconds: number | null) {
  if (milliseconds === null || !Number.isFinite(milliseconds)) return "-";
  if (milliseconds < 1000) return `${Math.round(milliseconds)} ms`;
  if (milliseconds < 60_000) return `${(milliseconds / 1000).toFixed(milliseconds < 10_000 ? 2 : 1)} s`;
  const minutes = Math.floor(milliseconds / 60_000);
  const seconds = Math.round((milliseconds % 60_000) / 1000);
  return `${minutes}m ${seconds}s`;
}
