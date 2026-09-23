import { useLayoutEffect, useMemo, useRef, useState } from "react";
import type { Locale } from "../../../i18n/runtime";
import { useI18n } from "../../../i18n/store";
import { useTooltip, type TooltipAnchor } from "../../../shared/ui/Tooltip";
import styles from "./ContributionCalendarChart.module.scss";

export type ContributionDay = {
  date: string;
  tokens: number;
};

const DAY_IN_MS = 24 * 60 * 60 * 1000;
const CELL_GAP = 3;
/** Cell edge in CSS pixels: the grid used to stretch its cells to the card width,
 *  which drew 13px squares across a 1280px page. It now grows only until the squares
 *  are comfortable to point at, then stops. */
const MIN_CELL_SIZE = 9;
const MAX_CELL_SIZE = 17;
const WEEKDAY_COLUMN = 30;
const ROUNDED = 2;

type Cell = ContributionDay & {
  column: number;
  row: number;
  level: number;
};

function parseDate(date: string) {
  return new Date(`${date}T00:00:00Z`);
}

/** Monday-first weekday index, which is how the calendar is read in both locales. */
function mondayIndex(date: Date) {
  return (date.getUTCDay() + 6) % 7;
}

function buildCalendar(data: ContributionDay[], locale: Locale) {
  if (data.length === 0) return null;
  const monthFormatter = new Intl.DateTimeFormat(locale, { month: "short", timeZone: "UTC" });
  const maximum = Math.max(1, ...data.map(({ tokens }) => tokens));
  const firstDate = parseDate(data[0].date);
  const gridStart = new Date(firstDate.getTime() - mondayIndex(firstDate) * DAY_IN_MS);
  const cells: Cell[] = data.map((day) => {
    const date = parseDate(day.date);
    const daysFromStart = Math.round((date.getTime() - gridStart.getTime()) / DAY_IN_MS);
    return {
      ...day,
      column: Math.floor(daysFromStart / 7),
      row: mondayIndex(date),
      level: day.tokens === 0 ? 0 : Math.max(1, Math.ceil((day.tokens / maximum) * 4)),
    };
  });
  const columnCount = cells.at(-1)!.column + 1;
  const monthTicks = cells.reduce<Array<{ key: string; text: string; column: number }>>((ticks, cell) => {
    const date = parseDate(cell.date);
    const key = `${date.getUTCFullYear()}-${date.getUTCMonth()}`;
    if (ticks.at(-1)?.key !== key) ticks.push({ key, text: monthFormatter.format(date), column: cell.column });
    return ticks;
  }, []);
  return { cells, columnCount, monthTicks };
}

/**
 * 过去一年的用量热力图。
 *
 * 画在 SVG 而不是 canvas 上：一格一个 <rect> 本来就是可聚焦、可悬停、可点击的元素，
 * 而 canvas 版本必须手工把鼠标坐标映射回格子、自己读主题色、自己做尺寸重算。
 * 371 个 rect 在一个桌面窗口里没有任何性能问题，换来的是一条真实的交互路径。
 */
export function ContributionCalendarChart({ data, onSelectDay }: {
  data: ContributionDay[];
  /** 点击某一天把页面范围收窄到那一天。 */
  onSelectDay?: (date: string) => void;
}) {
  const { locale } = useI18n();
  const { show, hide } = useTooltip();
  const frameRef = useRef<HTMLDivElement>(null);
  const [focused, setFocused] = useState<string | null>(null);
  const [cellSize, setCellSize] = useState(MIN_CELL_SIZE);
  const layout = useMemo(() => buildCalendar(data, locale), [data, locale]);
  const numberFormatter = useMemo(() => new Intl.NumberFormat(locale), [locale]);
  const weekdayFormatter = useMemo(() => new Intl.DateTimeFormat(locale, { weekday: "short", timeZone: "UTC" }), [locale]);
  const dateFormatter = useMemo(() => new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeZone: "UTC" }), [locale]);
  const columnCount = layout?.columnCount ?? 0;

  // The cell is sized from the space the card actually has: small enough for a year
  // to fit without a horizontal scrollbar, and capped so the squares stay something
  // a person can aim at.
  useLayoutEffect(() => {
    const node = frameRef.current;
    if (!node || columnCount === 0) return;
    const update = () => {
      const available = node.clientWidth - WEEKDAY_COLUMN;
      const size = Math.floor((available - (columnCount - 1) * CELL_GAP) / columnCount);
      setCellSize(Math.max(MIN_CELL_SIZE, Math.min(MAX_CELL_SIZE, size)));
    };
    const observer = new ResizeObserver(update);
    observer.observe(node);
    update();
    return () => observer.disconnect();
  }, [columnCount]);

  if (!layout) return null;
  const width = columnCount * (cellSize + CELL_GAP) - CELL_GAP;
  const height = 7 * (cellSize + CELL_GAP) - CELL_GAP;
  const label = (cell: Cell) => `${dateFormatter.format(parseDate(cell.date))} · ${t("Token 用量：{tokens}", { tokens: numberFormatter.format(cell.tokens) })}`;

  const revealTooltip = (element: SVGRectElement, cell: Cell) => {
    const anchor: TooltipAnchor = {
      contextElement: element,
      getBoundingClientRect: () => element.getBoundingClientRect(),
    };
    show(anchor, undefined, <div className={styles.tooltipContent}>
      <strong>{dateFormatter.format(parseDate(cell.date))}</strong>
      <span>{t("Token 用量：{tokens}", { tokens: numberFormatter.format(cell.tokens) })}</span>
      {onSelectDay && <span className={styles.tooltipHint}>{t("点击查看这一天的调用")}</span>}
    </div>);
  };

  return <div className={styles.root}>
    <div ref={frameRef} className={styles.frame}>
      <div className={styles.weekdays} aria-hidden="true" style={{ height }}>
        {[1, 3, 5].map((row) => <span key={row} style={{ top: row * (cellSize + CELL_GAP) - 5 }}>
          {weekdayFormatter.format(new Date(Date.UTC(2024, 0, 1 + row)))}
        </span>)}
      </div>
      <svg
        className={styles.grid}
        width={width}
        height={height + 16}
        viewBox={`0 0 ${width} ${height + 16}`}
        /* role="img" 会把所有后代从无障碍树里摘掉（img 的子节点被视为装饰），
           而每一格是可以 Tab 到、可以按回车选中的按钮——那样键盘用户会停在
           365 个没有任何名字的元素上。可交互时用 group，让每一格自己说话。 */
        role={onSelectDay ? "group" : "img"}
        aria-label={t("过去一年的 Token 用量日历")}
      >
        {layout.monthTicks.map((tick) => <text
          key={tick.key}
          className={styles.month}
          x={tick.column * (cellSize + CELL_GAP)}
          y={9}
        >{tick.text}</text>)}
        <g transform="translate(0 16)">
          {layout.cells.map((cell) => <rect
            key={cell.date}
            className={styles.cell}
            data-level={cell.level}
            data-dim={cell.tokens === 0 || undefined}
            data-focused={focused === cell.date || undefined}
            x={cell.column * (cellSize + CELL_GAP)}
            y={cell.row * (cellSize + CELL_GAP)}
            width={cellSize}
            height={cellSize}
            rx={ROUNDED}
            tabIndex={onSelectDay ? 0 : undefined}
            role={onSelectDay ? "button" : undefined}
            aria-label={label(cell)}
            onMouseEnter={(event) => revealTooltip(event.currentTarget, cell)}
            onMouseLeave={hide}
            onFocus={(event) => { setFocused(cell.date); revealTooltip(event.currentTarget, cell); }}
            onBlur={() => { setFocused(null); hide(); }}
            onClick={onSelectDay ? () => onSelectDay(cell.date) : undefined}
            onKeyDown={onSelectDay ? (event) => {
              if (event.key === "Enter" || event.key === " ") {
                event.preventDefault();
                onSelectDay(cell.date);
              }
            } : undefined}
          />)}
        </g>
      </svg>
    </div>
    <div className={styles.legend}>
      <span>{t("少")}</span>
      {[0, 1, 2, 3, 4].map((level) => <span key={level} className={styles.legendCell} data-level={level} />)}
      <span>{t("多")}</span>
      {onSelectDay && <span className={styles.legendHint}>{t("点一天看当天明细")}</span>}
    </div>
  </div>;
}
