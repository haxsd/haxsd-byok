import type { PricingEstimate } from "./peakOffPeakPricing";
import { formatCompactInteger, formatInteger } from "../../../shared/utils/numberFormat";
import { useAppStore } from "../../../shared/store/appStore";
import { useI18n } from "../../../i18n/store";
import { Icon } from "../../../shared/ui/Icon";
import { ProgressRing } from "../../../shared/ui/ProgressRing";
import { Sparkline } from "../../../shared/ui/Sparkline";
import { StatTile } from "../../../shared/ui/StatTile";
import { useTooltip, type TooltipAnchor } from "../../../shared/ui/Tooltip";
import { informationOutlineIcon } from "../../../shared/ui/icons";
import { currencyOf, formatCost, formatMoney } from "./tokenCost";
import type { BucketStat } from "../overview/overviewStats";
import styles from "./HomeMetrics.module.scss";

export type HomeMetricsData = {
  llmCalls: number;
  successfulCalls: number;
  failedCalls: number;
  tokenUsage: number;
  promptTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
};

function formatMetricValue(value: number) {
  const full = formatInteger(value);
  const compact = formatCompactInteger(value);
  return full === compact ? full : `${full} (${compact})`;
}

function formatRate(value: number | null) {
  return value === null ? t("暂无数据") : `${(Math.max(0, Math.min(1, value)) * 100).toFixed(2)}%`;
}

function calculateRate(numerator: number, denominator: number) {
  return denominator > 0 ? numerator / denominator : null;
}

function elementAnchor(element: HTMLElement): TooltipAnchor {
  return {
    contextElement: element,
    getBoundingClientRect: () => element.getBoundingClientRect(),
  };
}

function InfoTooltip({ content }: { content: string }) {
  const { show, hide } = useTooltip();

  return <button
    type="button"
    className={styles.info}
    aria-label={t("查看说明")}
    onMouseEnter={(event) => show(elementAnchor(event.currentTarget), undefined, <div className={styles.tooltipText}>{content}</div>)}
    onMouseLeave={hide}
    onFocus={(event) => show(elementAnchor(event.currentTarget), undefined, <div className={styles.tooltipText}>{content}</div>)}
    onBlur={hide}
  ><Icon icon={informationOutlineIcon} size="1.1em" /></button>;
}

/**
 * 首页的四个结论。
 *
 * 每个数字都配一条趋势线：图上的分桶数据本来就有，只是以前只有总和被取走，
 * 于是「这个月比上个月多吗」这种问题在界面上无迹可寻。
 */
export function HomeMetrics({ data, stats, callCounts, estimate = null, pricingFailure = null, refreshVersion = 0 }: {
  data: HomeMetricsData;
  stats: BucketStat[];
  callCounts: number[];
  /** Shared model-aware estimate also supplies the trend and model rows. */
  estimate?: PricingEstimate | null;
  /** Why there is no estimate: "range" (客户端区间上限) / "read" (读取失败)，null 表示正常。 */
  pricingFailure?: "range" | "read" | null;
  refreshVersion?: number;
}) {
  const { developerMode } = useAppStore();
  // 币种跟随界面语言：简体中文用人民币，英文用美元。
  const { locale } = useI18n();
  const currency = currencyOf(locale);
  const inputTokens = Math.max(0, data.promptTokens - data.cacheReadTokens - data.cacheWriteTokens);
  const outputTokens = Math.max(0, data.tokenUsage - data.promptTokens);
  const defaultCacheHitRate = calculateRate(data.cacheReadTokens, data.cacheReadTokens + inputTokens);
  const cacheReuseRate = calculateRate(
    data.cacheReadTokens,
    data.cacheReadTokens + data.cacheWriteTokens + inputTokens,
  );
  const successfulCallRate = calculateRate(data.successfulCalls, data.llmCalls);

  const pricedModels = estimate?.models.filter(row => row.cost !== null && row.tokens > 0) ?? [];
  const unpricedModels = estimate?.models.filter(row => row.cost === null && row.tokens > 0) ?? [];
  const totalCost = estimate?.total ?? 0;
  const cacheCost = (estimate?.cacheRead ?? 0) + (estimate?.cacheWrite ?? 0);
  const costValue = pricingFailure ? t("估算不可用") : !estimate ? t("计算中…") : unpricedModels.length > 0 && pricedModels.length === 0 ? t("未定价") : formatCost(totalCost, currency);
  const costHint = pricingFailure === "range"
    ? t("所选范围超过 660 天，未生成费用估算")
    : pricingFailure
      ? t("用量读取失败，未生成费用估算")
      : !estimate
        ? t("正在按模型读取用量")
        : unpricedModels.length > 0
          ? t("{count} 个模型未定价，{tokens} Token 未计入金额", { count: unpricedModels.length, tokens: formatCompactInteger(estimate?.unpricedTokens ?? 0) })
          : t("缓存读写 {cost}", { cost: formatCost(cacheCost, currency) });

  const cacheTooltip = developerMode
    ? [
      t("当前：{rate}", { rate: formatRate(defaultCacheHitRate) }),
      t("公式：缓存读取 /（缓存读取 + 非缓存输入）"),
      t("默认 {defaultRate} / 计入创建 {reuseRate}", {
        defaultRate: formatRate(defaultCacheHitRate),
        reuseRate: formatRate(cacheReuseRate),
      }),
    ].join("\n")
    // 关闭开发者模式时只留一句人话：口径与公式是排查用的，不是日常数字。
    : t("当前：{rate}", { rate: formatRate(defaultCacheHitRate) });
  const callsTooltip = [
    t("按历史 LLM 调用记录汇总，进行中的调用不计入。"),
    "",
    t("总调用：{count}", { count: formatMetricValue(data.llmCalls) }),
    t("成功调用：{count}", { count: formatMetricValue(data.successfulCalls) }),
    t("异常调用：{count}", { count: formatMetricValue(data.failedCalls) }),
    t("成功占比：{rate}", { rate: formatRate(successfulCallRate) }),
  ].join("\n");
  const tokensTooltip = [
    t("总请求 Token 包含提示词和模型输出。"),
    "",
    t("总请求：{tokens}", { tokens: formatMetricValue(data.tokenUsage) }),
    t("提示词：{tokens}", { tokens: formatMetricValue(data.promptTokens) }),
    t("输出推算：{tokens}", { tokens: formatMetricValue(outputTokens) }),
    t("非缓存输入：{tokens}", { tokens: formatMetricValue(inputTokens) }),
    t("缓存读取：{tokens}", { tokens: formatMetricValue(data.cacheReadTokens) }),
    t("缓存写入：{tokens}", { tokens: formatMetricValue(data.cacheWriteTokens) }),
    "",
    t("缓存读写已计入提示词侧统计。"),
  ].join("\n");
  const costTooltip = [
    t("按各模型的单价与调用时段分别估算后汇总。"),
    ...pricedModels.map(row => `${row.label}: ${formatCost(row.cost ?? 0, currency)}`),
    ...unpricedModels.map(row => t("{model}：未定价，{tokens} Token 未计入金额", { model: row.label, tokens: formatMetricValue(row.tokens) })),
    ...(developerMode && estimate ? [
      t("输入估算：{cost}", { cost: formatMoney(estimate.input, currency) }),
      t("输出估算：{cost}", { cost: formatMoney(estimate.output, currency) }),
      t("缓存读取估算：{cost}", { cost: formatMoney(estimate.cacheRead, currency) }),
      t("缓存写入估算：{cost}", { cost: formatMoney(estimate.cacheWrite, currency) }),
    ] : []),
    costHint,
    t("已定价小计：{cost}", { cost: estimate ? formatCost(totalCost, currency) : "—" }),
    t("修改单价会重新估算历史用量；这里不是服务商账单。"),
  ].join("\n");

  const trend = (values: number[]) => values.length < 2 ? undefined : values;
  const tokenTrend = trend(stats.map((stat) => stat.totalTokens));
  const costTrend = estimate && pricedModels.length > 0 ? trend(stats.map(stat => stat.cost)) : undefined;
  const callTrend = trend(callCounts);

  return <section className={styles.root} aria-label={t("调用统计")}>
    <StatTile
      label={t("缓存命中率")}
      info={<InfoTooltip content={cacheTooltip} />}
      value={formatRate(defaultCacheHitRate)}
      hint={t("缓存读取 /（缓存读取 + 非缓存输入）")}
      visualPosition="right"
      visual={<ProgressRing
        value={defaultCacheHitRate ?? 0}
        size={62}
        thickness={6}
        tone={(defaultCacheHitRate ?? 0) >= 0.5 ? "ok" : (defaultCacheHitRate ?? 0) > 0 ? "warn" : "bad"}
        animationKey={refreshVersion}
        ariaLabel={t("缓存命中率 {rate}", { rate: formatRate(defaultCacheHitRate) })}
      />}
    />
    <StatTile
      label={t("LLM 调用")}
      info={<InfoTooltip content={callsTooltip} />}
      value={formatCompactInteger(data.llmCalls)}
      title={formatInteger(data.llmCalls)}
      hint={t("成功 {successful} / 异常 {failed}", {
        successful: formatCompactInteger(data.successfulCalls),
        failed: formatCompactInteger(data.failedCalls),
      })}
      visual={callTrend && <Sparkline values={callTrend} tone="accent" ariaLabel={t("调用次数趋势")} />}
    />
    <StatTile
      label={t("Token 消耗")}
      info={<InfoTooltip content={tokensTooltip} />}
      value={formatCompactInteger(data.tokenUsage)}
      title={formatInteger(data.tokenUsage)}
      hint={t("提示词 {prompt} · 输出 {output}", {
        prompt: formatCompactInteger(data.promptTokens),
        output: formatCompactInteger(outputTokens),
      })}
      visual={tokenTrend && <Sparkline values={tokenTrend} tone="accent" ariaLabel={t("Token 用量趋势")} />}
    />
    <StatTile
      label={unpricedModels.length > 0 ? t("已定价小计") : t("价值估算")}
      info={<InfoTooltip content={costTooltip} />}
      value={costValue}
      hint={costHint}
      visual={costTrend && <Sparkline values={costTrend} tone="warn" ariaLabel={t("费用趋势")} />}
    />
  </section>;
}
