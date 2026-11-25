const RELATIVE_UNIT_MS: Record<string, number> = {
  分: 60_000,
  分钟: 60_000,
  min: 60_000,
  mins: 60_000,
  minute: 60_000,
  minutes: 60_000,
  мин: 60_000,
  минута: 60_000,
  минуту: 60_000,
  минуты: 60_000,
  минут: 60_000,
  小时: 60 * 60_000,
  h: 60 * 60_000,
  hour: 60 * 60_000,
  hours: 60 * 60_000,
  ч: 60 * 60_000,
  час: 60 * 60_000,
  часа: 60 * 60_000,
  часов: 60 * 60_000,
  天: 24 * 60 * 60_000,
  d: 24 * 60 * 60_000,
  day: 24 * 60 * 60_000,
  days: 24 * 60 * 60_000,
  д: 24 * 60 * 60_000,
  день: 24 * 60 * 60_000,
  дня: 24 * 60 * 60_000,
  дней: 24 * 60 * 60_000,
  周: 7 * 24 * 60 * 60_000,
  w: 7 * 24 * 60 * 60_000,
  week: 7 * 24 * 60 * 60_000,
  weeks: 7 * 24 * 60 * 60_000,
  нед: 7 * 24 * 60 * 60_000,
  неделя: 7 * 24 * 60 * 60_000,
  недели: 7 * 24 * 60 * 60_000,
  неделю: 7 * 24 * 60 * 60_000,
  недель: 7 * 24 * 60 * 60_000,
};

function localDate(parts: RegExpMatchArray) {
  const [, year, month, day, hour = "0", minute = "0", second = "0"] = parts;
  const value = new Date(+year, +month - 1, +day, +hour, +minute, +second);
  return value.getFullYear() === +year
    && value.getMonth() === +month - 1
    && value.getDate() === +day
    && value.getHours() === +hour
    && value.getMinutes() === +minute
    && value.getSeconds() === +second
    ? value.getTime()
    : null;
}

export function parseTimeInput(input: string, now = new Date()): number | null {
  const value = input.trim().toLowerCase();
  if (!value) return null;
  if (value === "现在" || value === "now" || value === "сейчас") return now.getTime();

  if (value === "今天" || value === "today" || value === "сегодня") {
    const start = new Date(now);
    start.setHours(0, 0, 0, 0);
    return start.getTime();
  }
  if (value === "昨天" || value === "yesterday" || value === "вчера") {
    const start = new Date(now);
    start.setDate(start.getDate() - 1);
    start.setHours(0, 0, 0, 0);
    return start.getTime();
  }

  const relative = value.match(/^(\d+(?:\.\d+)?)\s*(分钟|分|mins?|minutes?|min|минут[ауы]?|мин|小时|hours?|h|час(?:а|ов)?|ч|天|days?|d|дн(?:я|ей)|день|д|周|weeks?|w|недел[яиюь]|нед)\s*(?:前|ago|назад)$/);
  if (relative) return now.getTime() - Number(relative[1]) * RELATIVE_UNIT_MS[relative[2]];

  if (/^\d{10}$/.test(value)) return Number(value) * 1_000;
  if (/^\d{13}$/.test(value)) return Number(value);

  const normalized = value
    .replace(/年|\//g, "-")
    .replace(/月/g, "-")
    .replace(/日/g, " ")
    .replace(/时/g, ":")
    .replace(/分(?=\d)/g, ":")
    .replace(/分/g, "")
    .replace(/秒/g, "")
    .trim();
  const local = normalized.match(/^(\d{4})[-.](\d{1,2})[-.](\d{1,2})(?:[ t]+(\d{1,2})(?::(\d{1,2}))?(?::(\d{1,2}))?)?$/);
  if (local) return localDate(local);

  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

export function formatTimeInput(date: Date) {
  const part = (number: number) => String(number).padStart(2, "0");
  return `${date.getFullYear()}-${part(date.getMonth() + 1)}-${part(date.getDate())} ${part(date.getHours())}:${part(date.getMinutes())}`;
}

export type TimeRangeProblem = "start-unparsed" | "end-unparsed" | "order";

/**
 * 自定义区间的校验结果。
 *
 * 界面原来只把「应用」按钮灰掉：用户看不出是哪个输入框有问题、还是区间反了。返回值区分
 * 三种情况，界面据此给出能照着改的说法。
 */
export function timeRangeProblem(start: string, end: string, now = new Date()): TimeRangeProblem | null {
  const startMs = parseTimeInput(start, now);
  if (startMs === null) return "start-unparsed";
  const endMs = parseTimeInput(end, now);
  if (endMs === null) return "end-unparsed";
  return startMs >= endMs ? "order" : null;
}
