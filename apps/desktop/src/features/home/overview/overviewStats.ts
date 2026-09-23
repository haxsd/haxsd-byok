/**
 * 概览页的派生数据。
 *
 * 这一层存在的理由：首页要回答的不是「某个总数是多少」，而是「这些数字是怎么
 * 分布的」——哪一天用的多、调用落在哪几个模型上、缓存命中率是稳定的还是抖动的。
 * 这些都需要把同一份分桶数据按不同口径再算一遍，所以集中在这里，页面只负责画。
 */
import type {
  CurrencyPricing,
  LlmCall,
  OverviewTokenUsageBucket,
  TokenUsageGranularity,
} from "../../../shared/api";
import { priceAt } from "../metrics/peakOffPeakPricing";
import { priceTokens } from "../metrics/tokenCost";

export type BucketStat = {
  startMs: number;
  totalTokens: number;
  inputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  outputTokens: number;
  /** 缓存读取 /（缓存读取 + 非缓存输入）；没有输入时为 null。 */
  cacheHitRate: number | null;
  cost: number;
};

/** 单个分桶的用量与费用。费用按分桶起始时刻所属时段计价。 */
export function bucketStats(series: OverviewTokenUsageBucket[], pricing: CurrencyPricing): BucketStat[] {
  return series.map((bucket) => {
    const inputTokens = bucket.input_tokens;
    const cacheReadTokens = bucket.cache_read_tokens;
    const cacheWriteTokens = bucket.cache_write_tokens;
    const outputTokens = bucket.output_tokens;
    const price = priceAt(pricing, bucket.bucket_start_ms);
    return {
      startMs: bucket.bucket_start_ms,
      totalTokens: inputTokens + cacheReadTokens + cacheWriteTokens + outputTokens,
      inputTokens,
      cacheReadTokens,
      cacheWriteTokens,
      outputTokens,
      cacheHitRate: cacheReadTokens + inputTokens > 0 ? cacheReadTokens / (cacheReadTokens + inputTokens) : null,
      cost: priceTokens(inputTokens, price.input_per_million)
        + priceTokens(outputTokens, price.output_per_million)
        + priceTokens(cacheReadTokens, price.cache_read_per_million)
        + priceTokens(cacheWriteTokens, price.cache_write_per_million),
    };
  });
}

/** 把时间戳对齐到该粒度的分桶起点，与服务端的分桶口径一致。 */
export function bucketStart(epochMs: number, granularity: TokenUsageGranularity): number {
  const at = new Date(epochMs);
  if (granularity === "minute") {
    at.setUTCSeconds(0, 0);
    return at.getTime();
  }
  if (granularity === "hour") {
    at.setUTCMinutes(0, 0, 0);
    return at.getTime();
  }
  at.setUTCHours(0, 0, 0, 0);
  return at.getTime();
}

/** 把调用记录按同一组分桶计数，用来给「调用次数」画趋势。 */
export function callsPerBucket(series: OverviewTokenUsageBucket[], calls: LlmCall[], granularity: TokenUsageGranularity): number[] {
  if (series.length === 0) return [];
  const index = new Map<number, number>();
  series.forEach((bucket, position) => index.set(bucket.bucket_start_ms, position));
  const counts = new Array<number>(series.length).fill(0);
  for (const call of calls) {
    const position = index.get(bucketStart(call.created_at_ms, granularity));
    if (position !== undefined) counts[position] += 1;
  }
  return counts;
}

export type ModelUsageRow = {
  key: string;
  label: string;
  tokens: number;
  calls: number;
  share: number;
};

/**
 * 按模型汇总范围内的调用。
 *
 * 用调用记录而不是模型库：模型库只知道哪些模型配置好了，回答不了「谁在干活」。
 */
export function modelUsage(series: OverviewTokenUsageBucket[], calls: LlmCall[], granularity: TokenUsageGranularity): ModelUsageRow[] {
  const range = new Set(series.map((bucket) => bucket.bucket_start_ms));
  const rows = new Map<string, { label: string; tokens: number; calls: number }>();
  let total = 0;
  for (const call of calls) {
    if (series.length > 0 && !range.has(bucketStart(call.created_at_ms, granularity))) continue;
    const key = call.model_hash ?? call.model_id;
    const tokens = call.total_tokens ?? (call.input_tokens ?? 0) + (call.output_tokens ?? 0);
    const row = rows.get(key) ?? { label: call.display_name || call.model_id, tokens: 0, calls: 0 };
    row.tokens += tokens;
    row.calls += 1;
    rows.set(key, row);
    total += tokens;
  }
  return [...rows.entries()]
    .map(([key, row]) => ({ key, ...row, share: total > 0 ? row.tokens / total : 0 }))
    .sort((left, right) => right.tokens - left.tokens);
}

/** 范围内用量最高的一天，用来在图表的副标题里说清峰值落在哪。 */
export function peakDay(stats: BucketStat[]): BucketStat | null {
  return stats.reduce<BucketStat | null>((peak, stat) => stat.totalTokens > (peak?.totalTokens ?? 0) ? stat : peak, null);
}

/** 分桶数据的合计，避免把明细再加一遍。 */
export function sumStats(stats: BucketStat[]) {
  return stats.reduce((total, stat) => ({
    totalTokens: total.totalTokens + stat.totalTokens,
    cost: total.cost + stat.cost,
    cacheReadTokens: total.cacheReadTokens + stat.cacheReadTokens,
    cacheWriteTokens: total.cacheWriteTokens + stat.cacheWriteTokens,
    inputTokens: total.inputTokens + stat.inputTokens,
    outputTokens: total.outputTokens + stat.outputTokens,
  }), { totalTokens: 0, cost: 0, cacheReadTokens: 0, cacheWriteTokens: 0, inputTokens: 0, outputTokens: 0 });
}
