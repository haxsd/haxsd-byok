/**
 * 概览页的派生数据。
 *
 * 这一层存在的理由：首页要回答的不是「某个总数是多少」，而是「这些数字是怎么
 * 分布的」——哪一天用的多、调用落在哪几个模型上、缓存命中率是稳定的还是抖动的。
 * 这些都需要把同一份分桶数据按不同口径再算一遍，所以集中在这里，页面只负责画。
 */
import type {
  OverviewTokenUsageBucket,
  TokenUsageGranularity,
} from "../../../shared/api";

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

/** Token totals keep their existing bins; monetary values come from the shared model estimate. */
export function bucketStats(series: OverviewTokenUsageBucket[], costs: ReadonlyMap<number, number>): BucketStat[] {
  return series.map((bucket) => {
    const inputTokens = bucket.input_tokens;
    const cacheReadTokens = bucket.cache_read_tokens;
    const cacheWriteTokens = bucket.cache_write_tokens;
    const outputTokens = bucket.output_tokens;
    return {
      startMs: bucket.bucket_start_ms,
      totalTokens: inputTokens + cacheReadTokens + cacheWriteTokens + outputTokens,
      inputTokens,
      cacheReadTokens,
      cacheWriteTokens,
      outputTokens,
      cacheHitRate: cacheReadTokens + inputTokens > 0 ? cacheReadTokens / (cacheReadTokens + inputTokens) : null,
      cost: costs.get(bucket.bucket_start_ms) ?? 0,
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

/**
 * 每个分桶里的调用次数，用来给「调用次数」画趋势。
 *
 * 数的是分桶自带的模型明细，不是 `calls` 明细：调用列表只返回服务端截断后的最新一批，
 * 拿它按时间戳归桶时，范围一长就会漏掉窗口外的调用，而且调用页每 2 秒刷新一次明细，
 * 同一个区间的趋势还会跟着那个窗口自己变。
 */
export function callsPerBucket(series: OverviewTokenUsageBucket[]): number[] {
  return series.map((bucket) => bucket.models.reduce((count, model) => count + model.llm_calls, 0));
}

export type ModelUsageRow = {
  key: string;
  label: string;
  tokens: number;
  calls: number;
  share: number;
  /** Absent while loading; null is explicitly unpriced. */
  cost?: number | null;
};

/**
 * 按模型汇总范围内的用量。
 *
 * 用分桶自带的模型明细（服务端按整段范围聚合，已带上模型身份），而不是 `calls` 明细：
 * 调用列表只返回最新一批，用它算"这段时间里每个模型承担了多少"会漏掉窗口外的调用。
 * 没有配价时这条是「模型用量」卡的数据源；配了价则用估价那边同样的分桶明细。
 */
export function modelUsage(series: OverviewTokenUsageBucket[]): ModelUsageRow[] {
  const rows = new Map<string, { label: string; tokens: number; calls: number }>();
  let total = 0;
  for (const bucket of series) {
    for (const model of bucket.models) {
      const key = model.model_hash ?? model.model_id;
      const tokens = model.input_tokens + model.cache_read_tokens + model.cache_write_tokens + model.output_tokens;
      const row = rows.get(key) ?? { label: model.display_name || model.model_id, tokens: 0, calls: 0 };
      row.tokens += tokens;
      row.calls += model.llm_calls;
      rows.set(key, row);
      total += tokens;
    }
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
