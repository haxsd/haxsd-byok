import assert from "node:assert/strict";
import test from "node:test";
import { API_ROOT, FakeBackend, installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());
const { sumHourlyCost, fetchHourlyUsage } = await loader.load("/src/features/home/metrics/peakOffPeakPricing.ts");
const monday = Date.UTC(2026, 9, 12, 2);
const saturday = Date.UTC(2026, 9, 17, 2);
const price = (input, output = 0, read = 0, write = 0) => ({ input_per_million: input, output_per_million: output, cache_read_per_million: read, cache_write_per_million: write });
const rule = (mode, peak, offPeak = peak) => ({ mode, cny: { fixed: peak, peak, off_peak: offPeak }, usd: { fixed: price(100), peak: price(100), off_peak: price(50) } });
const deepseek = rule("peak_off_peak", price(2, 8, 0.04), price(1, 4, 0.02));
const gpt = rule("fixed", price(10, 40, 1));
const usage = (hash, id, input = 1_000_000, output = 0, read = 0, write = 0) => ({ model_hash: hash, model_id: id, display_name: id, llm_calls: 1, input_tokens: input, output_tokens: output, cache_read_tokens: read, cache_write_tokens: write });
function bucket(at, models) {
  const totals = models.reduce((sum, item) => ({ input_tokens: sum.input_tokens + item.input_tokens, output_tokens: sum.output_tokens + item.output_tokens, cache_read_tokens: sum.cache_read_tokens + item.cache_read_tokens, cache_write_tokens: sum.cache_write_tokens + item.cache_write_tokens }), { input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0 });
  return { bucket_start_ms: at, ...totals, models };
}
// The obsolete global fields are a deliberate trap: they reproduce the current
// DeepSeek-for-everything bug. The new estimator must consume only models.
const prices = (models) => ({ ...deepseek.cny, models });

test("DeepSeek and GPT use their own input, output and cache prices before summing", () => {
  const result = sumHourlyCost([bucket(monday, [usage("ds", "deepseek", 1_000_000, 500_000, 1_000_000), usage("gpt", "gpt", 1_000_000, 500_000, 500_000)])], prices({ ds: deepseek, gpt }), "CNY");
  assert.equal(result.input, 12);
  assert.equal(result.output, 24);
  assert.equal(result.cacheRead, 0.54);
  assert.ok(Math.abs(result.total - 36.54) < 1e-10);
  assert.equal(result.models.length, 2);
});

test("a fixed GPT price never inherits DeepSeek weekend discounts", () => {
  const series = [monday, saturday].map(at => bucket(at, [usage("ds", "deepseek"), usage("gpt", "gpt")]));
  assert.equal(sumHourlyCost(series, prices({ ds: deepseek, gpt }), "CNY").input, 23);
});

test("missing model prices are excluded and explicitly counted, not filled from global prices", () => {
  const result = sumHourlyCost([bucket(monday, [usage("ds", "deepseek"), usage("gpt", "gpt")])], prices({ ds: deepseek }), "CNY");
  assert.equal(result.total, 2);
  assert.equal(result.unpricedTokens, 1_000_000);
  assert.equal(result.models.find(row => row.modelHash === "gpt").cost, null);
});

test("an explicitly zero-priced model is priced, unlike a missing rule", () => {
  const result = sumHourlyCost([bucket(monday, [usage("free", "free-model")])], prices({ free: rule("fixed", price(0)) }), "CNY");
  assert.equal(result.total, 0);
  assert.equal(result.unpricedTokens, 0);
  assert.equal(result.models[0].cost, 0);
});

test("a missing identity is not guessed from a model name", () => {
  const result = sumHourlyCost([bucket(monday, [usage(null, "deepseek")])], prices({ ds: deepseek }), "CNY");
  assert.equal(result.total, 0);
  assert.equal(result.unpricedTokens, 1_000_000);
});

test("currency selects each model's independent book, never adds CNY to USD", () => {
  const result = sumHourlyCost([bucket(monday, [usage("ds", "deepseek"), usage("gpt", "gpt")])], prices({ ds: deepseek, gpt }), "USD");
  assert.equal(result.total, 200);
});

test("model totals and hourly trend sum to the same unrounded estimate", () => {
  const series = [monday, monday + 3_600_000].map(at => bucket(at, [usage("gpt", "gpt", 400)]));
  const result = sumHourlyCost(series, prices({ gpt }), "CNY");
  assert.equal(result.total, 0.008);
  assert.equal(result.models.reduce((sum, row) => sum + (row.cost ?? 0), 0), result.total);
  assert.equal(result.hours.reduce((sum, row) => sum + row.cost, 0), result.total);
});

test("an unconfigured currency is unpriced, not a fabricated free price", () => {
  const result = sumHourlyCost([bucket(monday, [usage("gpt", "gpt")])], prices({ gpt: { ...gpt, usd: null } }), "USD");
  assert.equal(result.total, 0);
  assert.equal(result.unpricedTokens, 1_000_000);
  assert.equal(result.models[0].cost, null);
});

test("historical names and upstream IDs keep the configured stable model price", () => {
  const result = sumHourlyCost([bucket(monday, [usage("gpt", "old-name")]), bucket(saturday, [usage("gpt", "new-name")])], prices({ gpt }), "CNY");
  assert.equal(result.total, 20);
  assert.equal(result.models.length, 1);
  assert.equal(result.models[0].calls, 2);
});

test("sub-hour cost bins preserve minute trends instead of spreading an hourly total", async () => {
  const { bucketStats } = await loader.load("/src/features/home/overview/overviewStats.ts");
  const series = [bucket(monday + 60_000, [usage("gpt", "gpt", 100_000)]), bucket(monday + 120_000, [usage("ds", "deepseek", 1_000_000)])];
  const result = sumHourlyCost(series, prices({ ds: deepseek, gpt }), "CNY");
  const stats = bucketStats(series, new Map(result.hours.map(item => [item.bucket_start_ms, item.cost])));
  assert.deepEqual(stats.map(item => item.cost), [1, 2]);
  assert.equal(stats.reduce((sum, item) => sum + item.cost, 0), result.total);
});

test("a range beyond the pricing lookback limit fails explicitly instead of dropping old usage", async () => {
  const backend = new FakeBackend().route("GET", `${API_ROOT}/overview`, () => ({ body: { token_usage_series: [] } }));
  backend.install();
  await assert.rejects(fetchHourlyUsage({ startMs: monday - 661 * 86_400_000, endMs: monday }, []), /660/);
  assert.equal(backend.requests.length, 0);
});
