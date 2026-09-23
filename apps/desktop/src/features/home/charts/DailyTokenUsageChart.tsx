import type { EChartsCoreOption } from "echarts/core";
import { useMemo, useState } from "react";
import type { TokenUsageGranularity } from "../../../shared/api";
import { useI18n } from "../../../i18n/store";
import { formatCompactInteger } from "../../../shared/utils/numberFormat";
import { EChart } from "./EChart";
import { chartPalette } from "./chartTheme";
import styles from "./DailyTokenUsageChart.module.scss";

export type DailyTokenUsage = {
  bucketStartMs: number;
  inputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  outputTokens: number;
};

type TooltipItem = {
  dataIndex: number;
};

/**
 * Zero-usage days are drawn as a thin baseline rather than a bar: a fraction of
 * the axis, so its pixel height stays constant however the data moves. It used to
 * be drawn at the full height of the tallest bar, which made a month with a single
 * active day read as a month where every day used the same amount.
 */
const EMPTY_BAR_RATIO = 0.014;

const seriesFocus = {
  emphasis: { focus: "series" },
  blur: { itemStyle: { opacity: 0.2 } },
} as const;

function colorMark(color: string): string {
  return `<span style="display:inline-block;width:8px;height:8px;margin-right:6px;border-radius:2px;background-color:${color};vertical-align:middle"></span>`;
}

function pad(value: number) {
  return String(value).padStart(2, "0");
}

function formatDay(value: Date) {
  return `${value.getUTCMonth() + 1}/${value.getUTCDate()}`;
}

function totalTokens(day: DailyTokenUsage) {
  return day.inputTokens + day.cacheReadTokens + day.cacheWriteTokens + day.outputTokens;
}

/**
 * Rounds an axis maximum up to a readable step, so the tick labels land on round
 * numbers instead of on whatever the busiest day happened to be.
 */
function niceMaximum(value: number) {
  if (value <= 0) return 1;
  const magnitude = 10 ** Math.floor(Math.log10(value));
  const normalized = value / magnitude;
  const step = [1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10].find((candidate) => normalized <= candidate) ?? 10;
  return step * magnitude;
}

export function DailyTokenUsageChart({
  data,
  granularity,
  onSelectBucket,
}: {
  data: DailyTokenUsage[];
  granularity: TokenUsageGranularity;
  /** Clicking a bar zooms the page's range to that bucket. */
  onSelectBucket?: (bucketStartMs: number) => void;
}) {
  const { locale } = useI18n();
  const [hovered, setHovered] = useState(false);
  // Read from the theme rather than module constants, so the bars follow a theme
  // change instead of keeping whatever colours this file was written with.
  const palette = chartPalette();
  const seriesColors = {
    input: palette.input,
    cacheRead: palette.cacheRead,
    cacheWrite: palette.cacheWrite,
    output: palette.output,
  };
  const monthFormatter = useMemo(() => new Intl.DateTimeFormat(locale, { month: "short", timeZone: "UTC" }), [locale]);
  const maximumTotal = data.reduce((maximum, day) => Math.max(maximum, totalTokens(day)), 0);
  const axisMaximum = niceMaximum(maximumTotal);
  const emptyBarHeight = axisMaximum * EMPTY_BAR_RATIO;
  const nonZeroTotals = data.map(totalTokens).filter((total) => total !== 0);
  const averageLevel = nonZeroTotals.length === 0
    ? 0
    : nonZeroTotals.reduce((sum, total) => sum + total, 0) / nonZeroTotals.length;

  /**
   * The stacked bar is only rounded where it ends. ECharts applies itemStyle per
   * series, so the rounding has to be attached to whichever band happens to be the
   * top one in that bucket; a fixed radius on the output series drew rounded corners
   * in the middle of a bar whenever a day had no output tokens.
   */
  const topBandPerBucket = useMemo(() => data.map((day) => {
    const bands = [day.inputTokens, day.cacheReadTokens, day.cacheWriteTokens, day.outputTokens];
    for (let index = bands.length - 1; index >= 0; index -= 1) {
      if (bands[index] > 0) return index;
    }
    return -1;
  }), [data]);

  const option = useMemo<EChartsCoreOption>(() => {
    const stackedSeries = (
      [
        ["input", t("输入（非缓存）"), (day: DailyTokenUsage) => day.inputTokens],
        ["cacheRead", t("缓存输入"), (day: DailyTokenUsage) => day.cacheReadTokens],
        ["cacheWrite", t("缓存写入"), (day: DailyTokenUsage) => day.cacheWriteTokens],
        ["output", t("模型输出"), (day: DailyTokenUsage) => day.outputTokens],
      ] as const
    ).map(([key, name, pick], bandIndex) => ({
      name,
      type: "bar" as const,
      stack: "tokens",
      data: data.map((day, bucketIndex) => ({
        value: pick(day),
        itemStyle: topBandPerBucket[bucketIndex] === bandIndex ? { borderRadius: [3, 3, 0, 0] } : undefined,
      })),
      barMaxWidth: 22,
      itemStyle: { color: seriesColors[key] },
      ...seriesFocus,
    }));

    return {
      animationDuration: 450,
      animationEasing: "cubicOut",
      grid: { top: 12, right: 10, bottom: 0, left: 0, containLabel: true },
      tooltip: {
        trigger: "axis",
        confine: true,
        backgroundColor: "var(--vscode-editorHoverWidget-background)",
        borderColor: "var(--vscode-editorHoverWidget-border)",
        textStyle: { color: "var(--vscode-foreground)", fontFamily: "PingFang-Medium" },
        // The tooltip is HTML, so unlike the bars it can read the theme tokens
        // directly instead of carrying its own radius and shadow.
        extraCssText: "border-radius: var(--oa-radius-md); box-shadow: var(--oa-tooltip-shadow); font-size: var(--daily-token-tooltip-font-size); line-height: 1.5;",
        axisPointer: {
          type: "shadow",
          shadowStyle: { color: palette.grid },
        },
        formatter: (params: unknown) => {
          const first = (params as TooltipItem[])[0];
          const day = data[first.dataIndex];
          const total = totalTokens(day);
          const line = (color: string, label: string, value: number) =>
            `${colorMark(color)}${label}：${formatCompactInteger(value)}${total > 0 && value > 0 ? `（${((value / total) * 100).toFixed(0)}%）` : ""}`;
          return [
            formatTooltipTime(day.bucketStartMs, granularity, locale),
            `${t("总请求")}：${formatCompactInteger(total)}`,
            line(seriesColors.input, t("输入（非缓存）"), day.inputTokens),
            line(seriesColors.cacheRead, t("缓存输入"), day.cacheReadTokens),
            line(seriesColors.cacheWrite, t("缓存写入"), day.cacheWriteTokens),
            line(seriesColors.output, t("模型输出"), day.outputTokens),
          ].join("<br/>");
        },
      },
      xAxis: {
        type: "category",
        data: data.map(({ bucketStartMs }) => bucketStartMs),
        axisTick: { show: false },
        axisLine: { lineStyle: { color: palette.grid } },
        axisLabel: {
          interval: "auto",
          hideOverlap: true,
          formatter: (_value: string, index: number) => formatAxisLabel(data[index].bucketStartMs, granularity, monthFormatter),
          color: palette.axis,
          fontFamily: "HFKos",
          margin: 14,
        },
      },
      yAxis: {
        type: "value",
        min: 0,
        max: axisMaximum,
        // Four steps keeps the labels on round numbers instead of letting the axis
        // engine add a final tick that breaks the interval.
        splitNumber: 4,
        axisLine: { show: false },
        axisTick: { show: false },
        splitLine: { lineStyle: { color: palette.grid } },
        axisLabel: {
          color: palette.axis,
          fontFamily: "HFKos",
          formatter: (value: number) => formatCompactInteger(value),
        },
      },
      series: [
        {
          name: t("无用量"),
          type: "bar",
          stack: "empty-placeholder",
          data: data.map((day) => totalTokens(day) === 0 ? emptyBarHeight : 0),
          barMaxWidth: 22,
          silent: true,
          z: 0,
          itemStyle: { color: palette.grid, borderRadius: 1 },
          emphasis: { disabled: true },
        },
        ...stackedSeries.map((series, bandIndex) => bandIndex === stackedSeries.length - 1 ? {
          ...series,
          barGap: "-100%",
          markLine: {
            silent: true,
            symbol: "none",
            lineStyle: { color: seriesColors.output, opacity: hovered ? 1 : 0, type: "dashed", width: 2 },
            label: {
              show: hovered,
              position: "insideStartTop",
              formatter: t("平均"),
              color: seriesColors.output,
              distance: 8,
            },
            data: [{ yAxis: averageLevel }],
          },
        } : series),
      ],
    };
  }, [averageLevel, axisMaximum, data, emptyBarHeight, granularity, hovered, monthFormatter, palette.grid, seriesColors.cacheRead, seriesColors.cacheWrite, seriesColors.input, seriesColors.output, topBandPerBucket]);

  return <div className={styles.root}>
    <EChart
      option={option}
      className={styles.chart}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      onClick={onSelectBucket ? (params) => {
        const index = (params as { dataIndex?: number }).dataIndex;
        if (typeof index === "number" && data[index]) onSelectBucket(data[index].bucketStartMs);
      } : undefined}
    />
    {/* A visible legend. Until now the only way to learn which colour was which was
        to hover a bar and read the tooltip, which is not a legend. */}
    <ul className={styles.legend} aria-label={t("图例")}>
      <li><span className={styles.swatch} style={{ background: seriesColors.input }} />{t("输入（非缓存）")}</li>
      <li><span className={styles.swatch} style={{ background: seriesColors.cacheRead }} />{t("缓存输入")}</li>
      <li><span className={styles.swatch} style={{ background: seriesColors.cacheWrite }} />{t("缓存写入")}</li>
      <li><span className={styles.swatch} style={{ background: seriesColors.output }} />{t("模型输出")}</li>
      {onSelectBucket && <li className={styles.hint}>{t("点击柱子可放大到该时段")}</li>}
    </ul>
  </div>;
}

function formatAxisLabel(bucketStartMs: number, granularity: TokenUsageGranularity, monthFormatter: Intl.DateTimeFormat) {
  const value = new Date(bucketStartMs);
  if (granularity === "minute") return `${pad(value.getHours())}:${pad(value.getMinutes())}`;
  if (granularity === "hour") return `${pad(value.getHours())}:00`;
  // The first of a month is labelled with the month: a month-long range reads as
  // weeks instead of as thirty near-identical dates.
  if (value.getUTCDate() === 1) return monthFormatter.format(value);
  return formatDay(value);
}

function formatTooltipTime(bucketStartMs: number, granularity: TokenUsageGranularity, locale: string) {
  const value = new Date(bucketStartMs);
  if (granularity === "day") {
    return new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeZone: "UTC" }).format(value);
  }
  const time = `${pad(value.getHours())}:${pad(value.getMinutes())}`;
  return `${value.getMonth() + 1}/${value.getDate()} ${time}`;
}
