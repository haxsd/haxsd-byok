import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { pluginText, type Overview, type OverviewTokenUsageBucket } from "../../shared/api";
import { requestRangeOverview } from "./overview/rangeOverviewRequest";
import { ContributionCalendarChart } from "./charts/ContributionCalendarChart";
import { DailyTokenUsageChart } from "./charts/DailyTokenUsageChart";
import { HomeMetrics } from "./metrics/HomeMetrics";
import { fetchHourlyUsage, sumHourlyCost } from "./metrics/peakOffPeakPricing";
import { currencyOf } from "./metrics/tokenCost";
import { PageContent } from "../../shell/layout/PageContent";
import type { VirtualPageSection } from "../../shell/layout/VirtualPage";
import { ModelUsageList } from "./overview/ModelUsageList";
import { bucketStart, bucketStats, callsPerBucket, modelUsage, peakDay, sumStats } from "./overview/overviewStats";
import { RecentCalls } from "./overview/RecentCalls";
import { OverviewTimeRangeFilter, type OverviewRangePreset, type QuickPreset } from "./overview/OverviewTimeRangeFilter";
import { PageActions } from "../../shell/PageActions";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { formatTimeInput, parseTimeInput } from "../../shared/utils/parseTimeInput";
import { modelProviderName } from "../../shared/utils/modelProvider";
import { formatCompactInteger } from "../../shared/utils/numberFormat";
import { PageTitle } from "../../shared/ui/PageTitle";
import { Button } from "../../shared/ui/Button";
import { TitledCard } from "../../shared/ui/TitledCard";
import { EmptyState } from "../../shared/ui/EmptyState";
import { claudeIcon, flatColorOrganizationIcon, openAiIcon, flatColorAreaChartIcon } from "../../shared/ui/icons";
import { useI18n } from "../../i18n/store";
import styles from "./HomePage.module.scss";

type TimeRange = { startMs: number; endMs: number };

const CALENDAR_DAYS = 365;
const DAY_MS = 24 * 60 * 60_000;

function contributionCalendarData(overview: Overview, endMs: number) {
  const tokensByDate = new Map<string, number>();
  for (const bucket of overview.token_usage_series) {
    const date = new Date(bucket.bucket_start_ms).toISOString().slice(0, 10);
    const tokens = bucket.input_tokens + bucket.cache_read_tokens + bucket.cache_write_tokens + bucket.output_tokens;
    tokensByDate.set(date, (tokensByDate.get(date) ?? 0) + tokens);
  }
  const lastDay = new Date(Math.max(0, endMs - 1));
  lastDay.setUTCHours(0, 0, 0, 0);
  const firstDayMs = lastDay.getTime() - (CALENDAR_DAYS - 1) * DAY_MS;
  return Array.from({ length: CALENDAR_DAYS }, (_, offset) => {
    const date = new Date(firstDayMs + offset * DAY_MS).toISOString().slice(0, 10);
    return { date, tokens: tokensByDate.get(date) ?? 0 };
  });
}

function presetRange(preset: Exclude<OverviewRangePreset, "custom">, now = new Date()): TimeRange {
  const endMs = now.getTime();
  if (preset === "today") {
    const start = new Date(now);
    start.setHours(0, 0, 0, 0);
    return { startMs: start.getTime(), endMs };
  }
  if (preset === "month") {
    const start = new Date(now);
    start.setMonth(start.getMonth() - 1);
    return { startMs: start.getTime(), endMs };
  }
  const duration = preset === "ten-minutes" ? 10 * 60_000
    : preset === "hour" ? 60 * 60_000
    : 7 * 24 * 60 * 60_000;
  return { startMs: now.getTime() - duration, endMs };
}

export function HomePage() {
  const { overview, calls, busy, models, plugins, pricing } = useAppStore();
  const { locale } = useI18n();
  const navigate = useNavigate();
  const [preset, setPreset] = useState<OverviewRangePreset>("month");
  const [quick, setQuick] = useState<QuickPreset | null>(null);
  const [customRange, setCustomRange] = useState<TimeRange | null>(null);
  const [customOpen, setCustomOpen] = useState(false);
  const [customStart, setCustomStart] = useState("");
  const [customEnd, setCustomEnd] = useState("");
  const [selectedModels, setSelectedModels] = useState<string[]>([]);
  const [appliedModels, setAppliedModels] = useState<string[]>([]);
  const [rangeOverview, setRangeOverview] = useState<Overview | null>(null);
  const [loadedRange, setLoadedRange] = useState<TimeRange | null>(null);
  const [rangeError, setRangeError] = useState<string | null>(null);
  const [rangeBusy, setRangeBusy] = useState(false);
  const [refreshVersion, setRefreshVersion] = useState(0);
  const [pricingSeries, setPricingSeries] = useState<OverviewTokenUsageBucket[] | null>(null);
  const [pricingSource, setPricingSource] = useState<TimeRange | null>(null);
  const [pricingFailure, setPricingFailure] = useState<"range" | "read" | null>(null);
  const selectedRange = preset === "custom" ? customRange : presetRange(preset);

  useEffect(() => {
    if (!selectedRange) return;
    setRangeBusy(true);
    setRangeOverview(null);
    setLoadedRange(null);
    // 区间请求的代际管理在 rangeOverviewRequest.ts：快速切换尺度/刷新时 abort 上一发，
    // 并且让旧的那一发回来后不再落地（失败时清掉区间数据并留下错误：`overview` 是全量
    // 数据，拿它兜底会把"近30天"的徽标挂到全量数字上——宁可什么都不显示，也不能显示
    // 错口径的数字）。
    return requestRangeOverview({ range: selectedRange, modelHashes: appliedModels }, {
      onLoaded: (next, range) => {
        setRangeOverview(next);
        setLoadedRange(range);
        setRangeError(null);
      },
      onFailed: (message) => {
        setRangeOverview(null);
        setRangeError(message);
      },
      onSettled: () => setRangeBusy(false),
    });
    // overview 只在渲染里当兜底数据用，不参与这次请求；把它列进依赖会让每次
    // store 刷新都重复抓同一段范围。
  }, [preset, customRange, refreshVersion, appliedModels]);

  // Sub-day bins already retain model identity and correct pricing periods.
  // Day bins need hour-level reads, using the exact same accepted time range.
  useEffect(() => {
    setPricingSeries(null);
    setPricingSource(null);
    setPricingFailure(null);
    if (!loadedRange || rangeOverview?.token_usage_granularity !== "day") return;
    let active = true;
    void fetchHourlyUsage(loadedRange, appliedModels)
      .then(series => {
        if (active) {
          setPricingSeries(series);
          setPricingSource(loadedRange);
        }
      })
      .catch((cause: unknown) => {
        // 区间超出取数上限是客户端自己的限制，不能说成"用量读取失败"。
        if (active) setPricingFailure(cause instanceof RangeError ? "range" : "read");
      });
    return () => { active = false; };
  }, [rangeOverview, loadedRange, appliedModels]);

  const filteredOverview = rangeOverview ?? overview;
  const granularity = filteredOverview.token_usage_granularity;
  const dailyTokenUsage = filteredOverview.token_usage_series.map((bucket) => ({
    bucketStartMs: bucket.bucket_start_ms,
    inputTokens: bucket.input_tokens,
    cacheReadTokens: bucket.cache_read_tokens,
    cacheWriteTokens: bucket.cache_write_tokens,
    outputTokens: bucket.output_tokens,
  }));
  // The calendar is labelled as the past year, so it reads the unfiltered overview.
  // Feeding it the range-filtered series left eleven of the twelve months empty
  // whenever a short range was selected, which is the default.
  const contribution = contributionCalendarData(overview, Date.now());
  const metrics = {
    llmCalls: filteredOverview.metrics.llm_calls,
    successfulCalls: filteredOverview.metrics.successful_calls,
    failedCalls: filteredOverview.metrics.failed_calls,
    tokenUsage: filteredOverview.metrics.token_usage,
    promptTokens: filteredOverview.metrics.prompt_tokens,
    cacheReadTokens: filteredOverview.metrics.cache_read_tokens,
    cacheWriteTokens: filteredOverview.metrics.cache_write_tokens,
  };
  const currency = currencyOf(locale);
  const pricingInput = !rangeOverview || rangeBusy ? null : granularity === "day" ? (pricingSource === loadedRange ? pricingSeries : null) : rangeOverview.token_usage_series;
  const estimate = useMemo(() => pricingInput === null ? null : sumHourlyCost(pricingInput, pricing, currency), [pricingInput, pricing, currency]);
  const costs = useMemo(() => {
    const result = new Map<number, number>();
    for (const hour of estimate?.hours ?? []) {
      const start = bucketStart(hour.bucket_start_ms, granularity);
      result.set(start, (result.get(start) ?? 0) + hour.cost);
    }
    return result;
  }, [estimate, granularity]);
  const stats = useMemo(() => bucketStats(filteredOverview.token_usage_series, costs), [filteredOverview.token_usage_series, costs]);
  const callCounts = useMemo(() => callsPerBucket(filteredOverview.token_usage_series), [filteredOverview.token_usage_series]);
  const modelRows = useMemo(() => estimate ? estimate.models.map(row => ({ ...row, share: estimate.totalTokens > 0 ? row.tokens / estimate.totalTokens : 0 })) : modelUsage(filteredOverview.token_usage_series), [estimate, filteredOverview.token_usage_series]);
  const totals = useMemo(() => sumStats(stats), [stats]);
  const busiest = useMemo(() => peakDay(stats), [stats]);
  const rangeIsEmpty = stats.length > 0 && stats.every((stat) => stat.totalTokens === 0);

  const openCustom = (open: boolean) => {
    if (open && !customStart && !customEnd) {
      const now = new Date();
      setCustomEnd(formatTimeInput(now));
      setCustomStart(formatTimeInput(new Date(now.getTime() - 60 * 60_000)));
    }
    setCustomOpen(open);
  };
  const applyCustom = () => {
    const startMs = parseTimeInput(customStart);
    const endMs = parseTimeInput(customEnd);
    if (startMs === null || endMs === null || startMs >= endMs) return;
    setCustomRange({ startMs, endMs });
    setAppliedModels(selectedModels);
    setQuick(null);
    setPreset("custom");
    setCustomOpen(false);
  };
  const selectQuick = (durationMs: number) => {
    const end = new Date();
    const start = new Date(end.getTime() - durationMs);
    setCustomStart(formatTimeInput(start));
    setCustomEnd(formatTimeInput(end));
    setCustomRange({ startMs: start.getTime(), endMs: end.getTime() });
    setAppliedModels(selectedModels);
    setQuick(durationMs === 4 * 60 * 60_000 ? "four-hours" : "twenty-four-hours");
    setPreset("custom");
    setCustomOpen(false);
  };
  const selectPreset = (value: Exclude<OverviewRangePreset, "custom">) => {
    setPreset(value);
    setQuick(null);
    setCustomOpen(false);
  };
  /** 图表和日历都支持把范围收到一个时间点上，这样「哪一天用的多」可以直接追问。 */
  const zoomToRange = (startMs: number, endMs: number) => {
    setCustomStart(formatTimeInput(new Date(startMs)));
    setCustomEnd(formatTimeInput(new Date(endMs)));
    setCustomRange({ startMs, endMs });
    setAppliedModels(selectedModels);
    setQuick(null);
    setPreset("custom");
  };
  const refresh = async () => {
    await appStore.refresh();
    setRefreshVersion((version) => version + 1);
  };
  const iconFor = (type: string) => type === "anthropic" ? claudeIcon : openAiIcon;
  const modelOptions = [
    ...models.map((model) => ({
      value: model.model_hash,
      label: model.display_name,
      group: modelProviderName(model),
      icon: iconFor(model.type),
    })),
    ...plugins.flatMap((plugin) => plugin.providers.flatMap((provider) =>
      provider.configured ? provider.models.filter((model) => model.enabled).map((model) => ({
        value: model.id,
        label: model.displayName,
        group: pluginText(provider.displayName, locale) || model.pluginName,
        iconSrc: model.icon || undefined,
        icon: model.icon ? undefined : flatColorOrganizationIcon,
      })) : [],
    )),
  ];

  const rangeLabel = presetLabel(preset, quick, customRange, locale);
  const usageDescription = stats.length === 0
    ? t("还没有这个范围的用量数据。")
    : rangeIsEmpty
      ? t("这个范围内没有调用记录。")
      : t("合计 {tokens} Token · 日均 {average} · 峰值 {peak}（{peakTokens}）", {
        tokens: formatCompactInteger(totals.totalTokens),
        average: formatCompactInteger(Math.round(totals.totalTokens / stats.length)),
        peak: busiest ? formatBucketLabel(busiest.startMs, granularity, locale) : "-",
        peakTokens: formatCompactInteger(busiest?.totalTokens ?? 0),
      });

  // 范围查询失败时，区间口径的数字一个都不渲染（指标、Token 图表、模型用量）：
  // 以前失败会静默回退到全量数据，徽标写着"近30天"、数字却是全量，用户以为看的是区间。
  const rangeSections: VirtualPageSection[] = rangeError ? [
    {
      key: "range-error",
      estimatedHeight: 170,
      content: <TitledCard
        title={t("时间范围数据")}
        description={t("所选范围的统计没有取到；这里不显示全量数据，避免与区间口径混淆。")}
        badge={<span className={styles.rangeBadge}>{rangeLabel}</span>}
      >
        <div className={styles.errorBlock}>
          <div>
            <strong>{t("这个范围的数据加载失败")}</strong>
            <small>{rangeError}</small>
          </div>
          <Button size="small" disabled={rangeBusy} onClick={() => setRefreshVersion((version) => version + 1)}>
            {rangeBusy ? t("重试中…") : t("重试")}
          </Button>
        </div>
      </TitledCard>,
    },
  ] : [
    {
      key: "metrics",
      estimatedHeight: 132,
      content: <HomeMetrics
        data={metrics}
        stats={stats}
        callCounts={callCounts}
        estimate={estimate}
        pricingFailure={granularity === "day" ? pricingFailure : null}
        refreshVersion={refreshVersion}
      />,
    },
    {
      key: "usage",
      estimatedHeight: 360,
      content: <TitledCard
        title={t("Token 用量")}
        description={usageDescription}
        badge={<span className={styles.rangeBadge}>{rangeLabel}</span>}
      >
        {rangeIsEmpty
          ? <div className={styles.emptyBlock}><EmptyState
            icon={flatColorAreaChartIcon}
            title={t("这个范围内还没有用量")}
            description={t("换一个时间范围，或先在 Cursor / Devin / Codex / Claude 里发起一次对话。")}
          /></div>
          : <div className={styles.chartBlock}>
            <DailyTokenUsageChart
              data={dailyTokenUsage}
              granularity={granularity}
              onSelectBucket={(bucketStartMs) => zoomToRange(bucketStartMs, bucketStartMs + bucketSpan(bucketStartMs, granularity))}
            />
          </div>}
      </TitledCard>,
    },
  ];
  const sections: VirtualPageSection[] = [
    ...rangeSections,
    {
      key: "calendar",
      estimatedHeight: 168,
      content: <TitledCard
        title={t("每日用量")}
        description={t("过去一年，每格一天；颜色越亮用量越高。")}
      >
        <div className={styles.calendarBlock}>
          <ContributionCalendarChart
            data={contribution}
            onSelectDay={(date) => {
              const startMs = Date.parse(`${date}T00:00:00Z`);
              zoomToRange(startMs, startMs + DAY_MS);
            }}
          />
        </div>
      </TitledCard>,
    },
    {
      key: "insight",
      estimatedHeight: 240,
      content: <div className={styles.twoColumn}>
        <TitledCard
          title={t("最近调用")}
          description={t("按时间倒序，点击一行查看完整请求。")}
          action={<button type="button" className={styles.linkButton} onClick={() => void navigate("/calls")}>{t("全部记录")}</button>}
        >
          <RecentCalls calls={calls} onOpen={(call) => void appStore.openCallDetails(call.call_id)} />
        </TitledCard>
        <TitledCard
          title={t("模型用量")}
          description={t("这段时间里，每个模型承担了多少。")}
        >
          {rangeError
            ? <div className={styles.emptyBlock}><small>{t("范围数据加载失败，暂时无法按模型统计。")}</small></div>
            : <ModelUsageList rows={modelRows} currency={currency} />}
        </TitledCard>
      </div>,
    },
  ];

  return <>
    <PageActions><OverviewTimeRangeFilter
      value={preset}
      quick={quick}
      customOpen={customOpen}
      customStart={customStart}
      customEnd={customEnd}
      modelOptions={modelOptions}
      selectedModels={selectedModels}
      busy={busy || rangeBusy}
      onSelect={selectPreset}
      onQuickSelect={selectQuick}
      onCustomOpenChange={openCustom}
      onCustomStartChange={setCustomStart}
      onCustomEndChange={setCustomEnd}
      onSelectedModelsChange={setSelectedModels}
      onCustomApply={applyCustom}
      onRefresh={() => void refresh()}
    /></PageActions>
    <PageContent
      title={<PageTitle title={t("概览")} meta={t("用量、费用与调用，按 {range} 汇总", { range: rangeLabel })} />}
      sections={sections}
    />;
  </>;
}

function bucketSpan(bucketStartMs: number, granularity: "minute" | "hour" | "day") {
  if (granularity === "minute") return 60_000;
  if (granularity === "hour") return 3_600_000;
  // 日分桶按 UTC 对齐，用下一天的起点相减可避开夏令时。
  const next = new Date(bucketStartMs);
  next.setUTCDate(next.getUTCDate() + 1);
  return next.getTime() - bucketStartMs;
}

function formatBucketLabel(bucketStartMs: number, granularity: "minute" | "hour" | "day", locale: string) {
  const date = new Date(bucketStartMs);
  if (granularity === "day") return new Intl.DateTimeFormat(locale, { month: "short", day: "numeric", timeZone: "UTC" }).format(date);
  return new Intl.DateTimeFormat(locale, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }).format(date);
}

function presetLabel(preset: OverviewRangePreset, quick: QuickPreset | null, customRange: TimeRange | null, locale: string) {
  if (preset === "custom" && customRange) {
    const formatter = new Intl.DateTimeFormat(locale, { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });
    return `${formatter.format(new Date(customRange.startMs))} – ${formatter.format(new Date(customRange.endMs))}`;
  }
  if (quick === "four-hours") return t("近4小时");
  if (quick === "twenty-four-hours") return t("近24小时");
  switch (preset) {
    case "ten-minutes": return t("近10分钟");
    case "hour": return t("近1小时");
    case "today": return t("近1自然日");
    case "week": return t("近一周");
    case "month": return t("近一个月");
    default: return t("自定义");
  }
}
