import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, pluginText, type Overview, type OverviewTokenUsageBucket } from "../../shared/api";
import { ContributionCalendarChart } from "./charts/ContributionCalendarChart";
import { DailyTokenUsageChart } from "./charts/DailyTokenUsageChart";
import { HomeMetrics } from "./metrics/HomeMetrics";
import { fetchHourlyUsage } from "./metrics/peakOffPeakPricing";
import { currencyOf, pricingFor } from "./metrics/tokenCost";
import { PageContent } from "../../shell/layout/PageContent";
import type { VirtualPageSection } from "../../shell/layout/VirtualPage";
import { ModelUsageList } from "./overview/ModelUsageList";
import { bucketStats, callsPerBucket, modelUsage, peakDay, sumStats } from "./overview/overviewStats";
import { RecentCalls } from "./overview/RecentCalls";
import { OverviewTimeRangeFilter, type OverviewRangePreset, type QuickPreset } from "./overview/OverviewTimeRangeFilter";
import { PageActions } from "../../shell/PageActions";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { formatTimeInput, parseTimeInput } from "../../shared/utils/parseTimeInput";
import { modelProviderName } from "../../shared/utils/modelProvider";
import { formatCompactInteger } from "../../shared/utils/numberFormat";
import { PageTitle } from "../../shared/ui/PageTitle";
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
  const [rangeBusy, setRangeBusy] = useState(false);
  const [refreshVersion, setRefreshVersion] = useState(0);
  const [pricingSeries, setPricingSeries] = useState<OverviewTokenUsageBucket[] | null>(null);
  const selectedRange = preset === "custom" ? customRange : presetRange(preset);

  useEffect(() => {
    if (!selectedRange) return;
    let active = true;
    setRangeBusy(true);
    void api.overview({
      ...selectedRange,
      modelHashes: appliedModels,
    }).then((next) => {
      if (active) setRangeOverview(next);
    }).finally(() => {
      if (active) setRangeBusy(false);
    });
    return () => { active = false; };
    // overview 只在渲染里当兜底数据用，不参与这次请求；把它列进依赖会让每次
    // store 刷新都重复抓同一段范围。
  }, [preset, customRange, refreshVersion, appliedModels]);

  // 分时计价需要按小时聚合的用量；固定单价模式不需要额外请求。
  // 这里不把 selectedRange 放进依赖：非自定义范围每次都返回新对象，会导致重复请求。
  useEffect(() => {
    if (pricing.mode !== "peak_off_peak" || !selectedRange) {
      setPricingSeries(null);
      return;
    }
    let active = true;
    void fetchHourlyUsage(selectedRange, appliedModels)
      .then((series) => { if (active) setPricingSeries(series); })
      .catch(() => { if (active) setPricingSeries(null); });
    return () => { active = false; };
    // 同理：overview 不是这次请求的输入。
  }, [preset, customRange, refreshVersion, appliedModels, pricing.mode]);

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
  const priceBook = pricingFor(pricing, currencyOf(locale));
  const stats = useMemo(() => bucketStats(filteredOverview.token_usage_series, priceBook), [filteredOverview.token_usage_series, priceBook]);
  const callCounts = useMemo(() => callsPerBucket(filteredOverview.token_usage_series, calls, granularity), [calls, filteredOverview.token_usage_series, granularity]);
  const modelRows = useMemo(() => modelUsage(filteredOverview.token_usage_series, calls, granularity), [calls, filteredOverview.token_usage_series, granularity]);
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
      : t("合计 {tokens} Token · 日均 {average} · 峰值在 {peak}（{peakTokens}）", {
        tokens: formatCompactInteger(totals.totalTokens),
        average: formatCompactInteger(Math.round(totals.totalTokens / stats.length)),
        peak: busiest ? formatBucketLabel(busiest.startMs, granularity, locale) : "-",
        peakTokens: formatCompactInteger(busiest?.totalTokens ?? 0),
      });

  const sections: VirtualPageSection[] = [
    {
      key: "metrics",
      estimatedHeight: 132,
      content: <HomeMetrics
        data={metrics}
        stats={stats}
        callCounts={callCounts}
        pricingSeries={pricingSeries}
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
            description={t("换一个时间范围，或先在 Cursor / Devin 里发起一次对话。")}
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
          <ModelUsageList rows={modelRows} />
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
