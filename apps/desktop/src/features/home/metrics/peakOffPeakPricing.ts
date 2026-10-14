import { api, type CurrencyPricing, type OverviewTokenUsageBucket, type TokenPrice, type TokenPricingSettings } from "../../../shared/api";
import { priceTokens, pricingFor, type Currency } from "./tokenCost";

const HOUR_MS = 3_600_000;
const CHUNK_HOURS = 1320;
const MAX_LOOKBACK_DAYS = 660;

export type PricingPeriod = "peak" | "off_peak";
export type CostBreakdown = { input: number; output: number; cacheRead: number; cacheWrite: number };
export type ModelCostRow = {
  key: string;
  label: string;
  modelHash: string | null;
  tokens: number;
  calls: number;
  /** null means unpriced; an explicit zero price remains a priced cost. */
  cost: number | null;
};
export type PricingEstimate = CostBreakdown & {
  total: number;
  totalTokens: number;
  unpricedTokens: number;
  models: ModelCostRow[];
  hours: { bucket_start_ms: number; cost: number }[];
};

export function pricingPeriodAt(epochMs: number): PricingPeriod {
  const at = new Date(epochMs);
  const weekday = at.getUTCDay();
  if (weekday === 0 || weekday === 6) return "off_peak";
  const minutes = at.getUTCHours() * 60 + at.getUTCMinutes();
  return (minutes >= 60 && minutes < 240) || (minutes >= 360 && minutes < 600) ? "peak" : "off_peak";
}

export function priceAt(pricing: CurrencyPricing, epochMs: number): TokenPrice {
  return pricing[pricingPeriodAt(epochMs)];
}

/** One estimate owns the tile, time trend and model rows. Never price merged tokens. */
export function sumHourlyCost(series: OverviewTokenUsageBucket[], pricing: TokenPricingSettings, currency: Currency): PricingEstimate {
  const result: PricingEstimate = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0, totalTokens: 0, unpricedTokens: 0, models: [], hours: [] };
  const rows = new Map<string, ModelCostRow>();
  for (const bucket of series) {
    let hourlyCost = 0;
    for (const model of bucket.models) {
      const key = model.model_hash !== null ? `model:${model.model_hash}` : `unknown:${model.model_id}`;
      const rule = model.model_hash !== null && Object.hasOwn(pricing.models, model.model_hash) ? pricing.models[model.model_hash] : undefined;
      const book = rule ? pricingFor(rule, currency) : null;
      const tokens = model.input_tokens + model.output_tokens + model.cache_read_tokens + model.cache_write_tokens;
      const row = rows.get(key) ?? { key, label: model.display_name || model.model_id, modelHash: model.model_hash, tokens: 0, calls: 0, cost: book ? 0 : null };
      row.tokens += tokens;
      row.calls += model.llm_calls;
      result.totalTokens += tokens;
      if (rule && book) {
        const price = rule.mode === "fixed" ? book.fixed : priceAt(book, bucket.bucket_start_ms);
        const input = priceTokens(model.input_tokens, price.input_per_million);
        const output = priceTokens(model.output_tokens, price.output_per_million);
        const cacheRead = priceTokens(model.cache_read_tokens, price.cache_read_per_million);
        const cacheWrite = priceTokens(model.cache_write_tokens, price.cache_write_per_million);
        const cost = input + output + cacheRead + cacheWrite;
        result.input += input;
        result.output += output;
        result.cacheRead += cacheRead;
        result.cacheWrite += cacheWrite;
        row.cost = (row.cost ?? 0) + cost;
        hourlyCost += cost;
      } else {
        result.unpricedTokens += tokens;
      }
      rows.set(key, row);
    }
    result.hours.push({ bucket_start_ms: bucket.bucket_start_ms, cost: hourlyCost });
    result.total += hourlyCost;
  }
  result.models = [...rows.values()].sort((left, right) => right.tokens - left.tokens);
  return result;
}

/** Chunk on aligned hour boundaries so neither usage nor model identity is lost. */
export async function fetchHourlyUsage(range: { startMs: number; endMs: number }, modelHashes: string[]): Promise<OverviewTokenUsageBucket[]> {
  const { startMs, endMs } = range;
  if (startMs < endMs - MAX_LOOKBACK_DAYS * 24 * HOUR_MS) {
    throw new RangeError("Pricing range exceeds 660 days");
  }
  const windows: { startMs: number; endMs: number }[] = [];
  let cursor = startMs;
  if (cursor < endMs && cursor % HOUR_MS !== 0) {
    const boundary = Math.ceil(cursor / HOUR_MS) * HOUR_MS;
    windows.push({ startMs: cursor, endMs: Math.min(boundary, endMs) });
    cursor = boundary;
  }
  const windowMs = CHUNK_HOURS * HOUR_MS;
  for (; cursor < endMs; cursor += windowMs) windows.push({ startMs: cursor, endMs: Math.min(cursor + windowMs, endMs) });
  const responses = await Promise.all(windows.map(window => api.overview({ startMs: window.startMs, endMs: window.endMs, modelHashes, bucketMs: HOUR_MS })));
  return responses.flatMap(overview => overview.token_usage_series);
}
