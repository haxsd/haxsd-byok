import type { OverviewTokenUsageBucket } from "../../../shared/api";
import { formatCompactInteger, formatInteger } from "../../../shared/utils/numberFormat";
import { useAppStore } from "../../../shared/store/appStore";
import { useI18n } from "../../../i18n/store";
import { Icon } from "../../../shared/ui/Icon";
import { ProgressRing } from "../../../shared/ui/ProgressRing";
import { Sparkline } from "../../../shared/ui/Sparkline";
import { StatTile } from "../../../shared/ui/StatTile";
import { useTooltip, type TooltipAnchor } from "../../../shared/ui/Tooltip";
import { informationOutlineIcon } from "../../../shared/ui/icons";
import { priceAt, sumHourlyCost } from "./peakOffPeakPricing";
import { currencyOf, currencySymbol, formatMoney, formatPrice, priceTokens, pricingFor } from "./tokenCost";
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

/** 把费用折算成每百万 token 的均价，使提示框里的算式始终成立。 */
function averagePrice(cost: number, tokens: number) {
  return tokens > 0 ? (cost / tokens) * 1_000_000 : 0;
}

/**
 * 不足一分钱的用量按「<$0.01」显示。
 *
 * 美元的单价低，一段真实用量的估算值经常落在 0 到 0.01 之间，两位小数会把它显示成
 * $0.00——那不是「没花钱」，而是「花了但显示不出来」，两者必须区分。
 */
function formatCost(value: number, currency: Parameters<typeof formatMoney>[1]) {
  if (value > 0 && value < 0.01) return `<${formatMoney(0.01, currency)}`;
  return formatMoney(value, currency);
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
export function HomeMetrics({ data, stats, callCounts, pricingSeries = null, refreshVersion = 0 }: {
  data: HomeMetricsData;
  /** 当前范围的逐分桶统计，用于趋势线。 */
  stats: BucketStat[];
  /** 与 stats 对齐的逐分桶调用次数。 */
  callCounts: number[];
  /** 按小时聚合的用量分桶；分时计价就绪时为数组，否则为 null。 */
  pricingSeries?: OverviewTokenUsageBucket[] | null;
  refreshVersion?: number;
}) {
  const { pricing } = useAppStore();
  // 币种跟随界面语言：简体中文用人民币，英文用美元。
  const { locale } = useI18n();
  const currency = currencyOf(locale);
  const priceBook = pricingFor(pricing, currency);
  const unit = currencySymbol(currency);
  const inputTokens = Math.max(0, data.promptTokens - data.cacheReadTokens - data.cacheWriteTokens);
  const outputTokens = Math.max(0, data.tokenUsage - data.promptTokens);
  const defaultCacheHitRate = calculateRate(data.cacheReadTokens, data.cacheReadTokens + inputTokens);
  const cacheReuseRate = calculateRate(
    data.cacheReadTokens,
    data.cacheReadTokens + data.cacheWriteTokens + inputTokens,
  );
  const successfulCallRate = calculateRate(data.successfulCalls, data.llmCalls);

  // 分时计价：逐小时用「该小时所属时段」的单价计算。
  const hourlyCosts = pricing.mode === "peak_off_peak" && pricingSeries?.length
    ? sumHourlyCost(pricingSeries, priceBook)
    : null;
  // 分桶数据尚未就绪时，先按当前时段的价格给出一个近似值（提示框会注明）。
  const fallbackPrice = pricing.mode === "peak_off_peak" ? priceAt(priceBook, Date.now()) : priceBook.fixed;
  const costs = hourlyCosts ?? {
    input: priceTokens(inputTokens, fallbackPrice.input_per_million),
    output: priceTokens(outputTokens, fallbackPrice.output_per_million),
    cacheRead: priceTokens(data.cacheReadTokens, fallbackPrice.cache_read_per_million),
    cacheWrite: priceTokens(data.cacheWriteTokens, fallbackPrice.cache_write_per_million),
  };
  const totalCost = costs.input + costs.output + costs.cacheRead + costs.cacheWrite;
  const cacheCost = costs.cacheRead + costs.cacheWrite;

  const cacheTooltip = [
    t("当前：{rate}", { rate: formatRate(defaultCacheHitRate) }),
    t("公式：缓存读取 /（缓存读取 + 非缓存输入）"),
    t("默认 {defaultRate} / 计入创建 {reuseRate}", {
      defaultRate: formatRate(defaultCacheHitRate),
      reuseRate: formatRate(cacheReuseRate),
    }),
  ].join("\n");
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
  const pricingNote = pricing.mode === "peak_off_peak"
    ? [
      hourlyCosts
        ? t("按高峰 / 低谷时段逐小时计价。")
        : t("正在读取按小时的用量，暂时按当前时段估价。"),
      t("规则：UTC 周一至周五 01:00-04:00、06:00-10:00 为高峰期，其余时段（含周末）为低谷期，低谷价 = 高峰价 5 折。"),
      t("下方单价为所选范围内的加权平均价。"),
    ]
    : [t("按配置的 Token 价格估算。")];
  const costTooltip = [
    ...pricingNote,
    t("缓存统计策略：默认口径（{rate}）", { rate: formatRate(defaultCacheHitRate) }),
    "",
    t("普通输入：{tokens} × {unit}{price}/1M = {cost}", {
      tokens: formatMetricValue(inputTokens),
      unit,
      price: formatPrice(averagePrice(costs.input, inputTokens)),
      cost: formatMoney(costs.input, currency),
    }),
    t("模型输出：{tokens} × {unit}{price}/1M = {cost}", {
      tokens: formatMetricValue(outputTokens),
      unit,
      price: formatPrice(averagePrice(costs.output, outputTokens)),
      cost: formatMoney(costs.output, currency),
    }),
    t("缓存读取：{tokens} × {unit}{price}/1M = {cost}", {
      tokens: formatMetricValue(data.cacheReadTokens),
      unit,
      price: formatPrice(averagePrice(costs.cacheRead, data.cacheReadTokens)),
      cost: formatMoney(costs.cacheRead, currency),
    }),
    t("缓存写入：{tokens} × {unit}{price}/1M = {cost}", {
      tokens: formatMetricValue(data.cacheWriteTokens),
      unit,
      price: formatPrice(averagePrice(costs.cacheWrite, data.cacheWriteTokens)),
      cost: formatMoney(costs.cacheWrite, currency),
    }),
    "",
    t("合计：{cost}", { cost: formatMoney(totalCost, currency) }),
  ].join("\n");

  const trend = (values: number[]) => values.length < 2 ? undefined : values;
  const tokenTrend = trend(stats.map((stat) => stat.totalTokens));
  const costTrend = trend(stats.map((stat) => stat.cost));
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
      label={t("价值估算")}
      info={<InfoTooltip content={costTooltip} />}
      value={formatCost(totalCost, currency)}
      hint={t("缓存读写 {cost}", { cost: formatCost(cacheCost, currency) })}
      visual={costTrend && <Sparkline values={costTrend} tone="warn" ariaLabel={t("费用趋势")} />}
    />
  </section>;
}
