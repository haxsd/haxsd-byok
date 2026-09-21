/**
 * 高峰 / 低谷分时计价。
 *
 * DeepSeek 按时段定价：高峰时段为 **UTC 周一至周五 01:00-04:00 与 06:00-10:00**，
 * 其余时间（含周末全天）为低谷时段，单价为高峰的一半。
 *
 * 首页的「价值估算」如果只把整段时间范围的 token 总数乘以打开界面那一刻的单价，
 * 同一范围在不同时间打开会得到不同金额，跨时段时也必然算错。这里的做法是：
 * 向本地服务按**小时**拉取用量分桶，每个小时用它**自己所属时段**的单价计算后求和。
 *
 * 之所以能精确：DeepSeek 的时段边界都落在整点上，而服务端的分桶也严格对齐整点，
 * 因此每个分桶完整地属于某个时段，不存在归属误差。
 */
import {
  api,
  type CurrencyPricing,
  type OverviewTokenUsageBucket,
  type TokenPrice,
} from "../../../shared/api";
import { priceTokens } from "./tokenCost";

/** 一小时的毫秒数。 */
const HOUR_MS = 3_600_000;
/**
 * 每个请求最多覆盖 1320 小时（55 天）。
 *
 * 服务端对显式指定的 `bucket_ms` 有分桶数量上限（1440 个），超出的部分只会返回
 * 最近 1440 个分桶，所以范围更长时必须自行分段，每段留在上限以内。
 */
const CHUNK_HOURS = 1320;
/** 最多向前追溯 660 天，避免极长的自定义范围发出过多请求。 */
const MAX_LOOKBACK_DAYS = 660;

/** 计价时段。 */
export type PricingPeriod = "peak" | "off_peak";

/** 各项费用的拆分。 */
export type CostBreakdown = {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
};

/**
 * 判断某个时间点属于高峰还是低谷时段。
 *
 * 时段按 UTC 计算：周一至周五 01:00-04:00、06:00-10:00 为高峰，其余为低谷。
 */
export function pricingPeriodAt(epochMs: number): PricingPeriod {
  const at = new Date(epochMs);
  const weekday = at.getUTCDay();
  if (weekday === 0 || weekday === 6) return "off_peak";
  const minutes = at.getUTCHours() * 60 + at.getUTCMinutes();
  const isPeak = (minutes >= 60 && minutes < 240) || (minutes >= 360 && minutes < 600);
  return isPeak ? "peak" : "off_peak";
}

/** 取某个时间点适用的单价。 */
export function priceAt(pricing: CurrencyPricing, epochMs: number): TokenPrice {
  return pricing[pricingPeriodAt(epochMs)];
}

/**
 * 按小时分桶逐段计价，返回各项费用。
 *
 * 每个分桶用它起始时刻所属时段的单价计算，累加后即为该时间范围内的真实金额。
 * 传入的是**某个币种**的价格表，因此金额的币种由调用方决定。
 */
export function sumHourlyCost(
  series: OverviewTokenUsageBucket[],
  pricing: CurrencyPricing,
): CostBreakdown {
  const total: CostBreakdown = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
  for (const bucket of series) {
    const price = priceAt(pricing, bucket.bucket_start_ms);
    total.input += priceTokens(bucket.input_tokens, price.input_per_million);
    total.output += priceTokens(bucket.output_tokens, price.output_per_million);
    total.cacheRead += priceTokens(bucket.cache_read_tokens, price.cache_read_per_million);
    total.cacheWrite += priceTokens(bucket.cache_write_tokens, price.cache_write_per_million);
  }
  return total;
}

/**
 * 按小时拉取用量分桶。
 *
 * 超长范围会自动切成多段并发取数：第一段先补齐到整点，后续每段按整天切片，
 * 这样相邻两段的边界落在同一条分桶线上，既不重叠也不遗漏。
 */
export async function fetchHourlyUsage(
  range: { startMs: number; endMs: number },
  modelHashes: string[],
): Promise<OverviewTokenUsageBucket[]> {
  const { endMs } = range;
  const startMs = Math.max(range.startMs, endMs - MAX_LOOKBACK_DAYS * 24 * HOUR_MS);
  const windows: { startMs: number; endMs: number }[] = [];

  let cursor = startMs;
  if (cursor < endMs && cursor % HOUR_MS !== 0) {
    const boundary = Math.ceil(cursor / HOUR_MS) * HOUR_MS;
    windows.push({ startMs: cursor, endMs: Math.min(boundary, endMs) });
    cursor = boundary;
  }
  const windowMs = CHUNK_HOURS * HOUR_MS;
  for (; cursor < endMs; cursor += windowMs) {
    windows.push({ startMs: cursor, endMs: Math.min(cursor + windowMs, endMs) });
  }

  const responses = await Promise.all(
    windows.map((window) =>
      api.overview({
        startMs: window.startMs,
        endMs: window.endMs,
        modelHashes,
        bucketMs: HOUR_MS,
      }),
    ),
  );
  return responses.flatMap((overview) => overview.token_usage_series);
}
