import type { EChartsCoreOption } from "echarts/core";
import { useMemo, useState } from "react";
import type { TokenUsageGranularity } from "../../../shared/api";
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
  return `<span style="display:inline-block;width:8px;height:8px;margin-right:6px;border-radius:50%;background-color:${color};vertical-align:middle"></span>`;
}

function formatDay(value: Date) {
  return `${value.getUTCMonth() + 1}/${value.getUTCDate()}`;
}

function pad(value: number) {
  return String(value).padStart(2, "0");
}

function formatAxisLabel(bucketStartMs: number, granularity: TokenUsageGranularity) {
  const value = new Date(bucketStartMs);
  if (granularity === "minute") return `${pad(value.getHours())}:${pad(value.getMinutes())}`;
  if (granularity === "hour") return `${pad(value.getHours())}:00`;
  const day = value.getUTCDay();
  if (day === 6) return t("周六");
  if (day === 0) return t("周日");
  return formatDay(value);
}

function formatTooltipTime(bucketStartMs: number, granularity: TokenUsageGranularity) {
  const value = new Date(bucketStartMs);
  if (granularity === "day") return formatDay(value);
  const time = `${pad(value.getHours())}:${pad(value.getMinutes())}`;
  return `${value.getMonth() + 1}/${value.getDate()} ${time}`;
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
}: {
  data: DailyTokenUsage[];
  granularity: TokenUsageGranularity;
}) {
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
  const levelLineColor = palette.output;
  const emptyBarColor = palette.grid;
  const maximumTotal = data.reduce((maximum, day) => Math.max(maximum, totalTokens(day)), 0);
  const axisMaximum = niceMaximum(maximumTotal);
  const emptyBarHeight = axisMaximum * EMPTY_BAR_RATIO;
  const nonZeroTotals = data.map(totalTokens).filter((total) => total !== 0);
  const averageLevel = nonZeroTotals.length === 0
    ? 0
    : nonZeroTotals.reduce((sum, total) => sum + total, 0) / nonZeroTotals.length;

  const option = useMemo<EChartsCoreOption>(() => ({
    animationDuration: 450,
    animationEasing: "cubicOut",
    grid: { top: 8, right: 8, bottom: 0, left: 0, containLabel: true },
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
        return [
          formatTooltipTime(day.bucketStartMs, granularity),
          `${t("总请求")}：${formatCompactInteger(totalTokens(day))}`,
          `${colorMark(seriesColors.input)}${t("输入（非缓存）")}：${formatCompactInteger(day.inputTokens)}`,
          `${colorMark(seriesColors.cacheRead)}${t("缓存输入")}：${formatCompactInteger(day.cacheReadTokens)}`,
          `${colorMark(seriesColors.cacheWrite)}${t("缓存写入")}：${formatCompactInteger(day.cacheWriteTokens)}`,
          `${colorMark(seriesColors.output)}${t("模型输出")}：${formatCompactInteger(day.outputTokens)}`,
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
        formatter: (_value: string, index: number) => formatAxisLabel(data[index].bucketStartMs, granularity),
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
        barMaxWidth: 18,
        silent: true,
        z: 0,
        itemStyle: { color: emptyBarColor, borderRadius: 1 },
        emphasis: { disabled: true },
      },
      {
        name: t("输入（非缓存）"),
        type: "bar",
        stack: "tokens",
        data: data.map(({ inputTokens }) => inputTokens),
        barMaxWidth: 18,
        itemStyle: { color: seriesColors.input },
        ...seriesFocus,
      },
      {
        name: t("缓存输入"),
        type: "bar",
        stack: "tokens",
        data: data.map(({ cacheReadTokens }) => cacheReadTokens),
        barMaxWidth: 18,
        itemStyle: { color: seriesColors.cacheRead },
        ...seriesFocus,
      },
      {
        name: t("缓存写入"),
        type: "bar",
        stack: "tokens",
        data: data.map(({ cacheWriteTokens }) => cacheWriteTokens),
        barMaxWidth: 18,
        itemStyle: { color: seriesColors.cacheWrite },
        ...seriesFocus,
      },
      {
        name: t("模型输出"),
        type: "bar",
        stack: "tokens",
        data: data.map(({ outputTokens }) => outputTokens),
        barMaxWidth: 18,
        barGap: "-100%",
        itemStyle: { color: seriesColors.output, borderRadius: [2, 2, 0, 0] },
        markLine: {
          silent: true,
          symbol: "none",
          lineStyle: { color: levelLineColor, opacity: hovered ? 1 : 0, type: "dashed", width: 2 },
          label: {
            show: hovered,
            position: "insideStartTop",
            formatter: t("平均"),
            color: levelLineColor,
            distance: 8,
          },
          data: [{ yAxis: averageLevel }],
        },
        ...seriesFocus,
      },
    ],
  }), [averageLevel, axisMaximum, data, emptyBarHeight, granularity, hovered]);

  return <EChart
    option={option}
    className={styles.root}
    onMouseEnter={() => setHovered(true)}
    onMouseLeave={() => setHovered(false)}
  />;
}
