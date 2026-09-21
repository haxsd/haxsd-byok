/**
 * 计价相关的纯函数与币种规则。
 *
 * 首页「价值估算」的币种跟随界面语言：**简体中文用人民币，英文用美元**。
 * 两种币种在设置里各有一套完整价格（都是官方公布的原始价，不是汇率折算），
 * 这里只负责「当前语言用哪一套、显示什么符号」。
 */
import type { CurrencyPricing, TokenPricingSettings } from "../../../shared/api";
import type { Locale } from "../../../i18n/runtime";

/** 支持的币种。 */
export type Currency = "CNY" | "USD";

/** 各币种的显示符号。 */
const CURRENCY_SYMBOLS: Record<Currency, string> = {
  CNY: "¥",
  USD: "$",
};

/** 价格与 token 数量的换算基准：每百万 token。 */
const TOKENS_PER_UNIT = 1_000_000;

/** 界面语言对应的币种。 */
export function currencyOf(locale: Locale): Currency {
  return locale === "zh-CN" ? "CNY" : "USD";
}

/** 币种符号。 */
export function currencySymbol(currency: Currency) {
  return CURRENCY_SYMBOLS[currency];
}

/** 取某个币种下的整套价格。 */
export function pricingFor(pricing: TokenPricingSettings, currency: Currency): CurrencyPricing {
  return currency === "CNY" ? pricing.cny : pricing.usd;
}

/** 按「每百万 token 的单价」换算一批 token 的费用。 */
export function priceTokens(tokens: number, pricePerMillion: number) {
  return (tokens / TOKENS_PER_UNIT) * pricePerMillion;
}

/** 把金额格式化为带币种符号、保留两位小数的字符串。 */
export function formatMoney(value: number, currency: Currency) {
  return `${currencySymbol(currency)}${value.toFixed(2)}`;
}

/**
 * 把单价格式化为紧凑字符串（不含币种符号，符号由调用方另外拼接）。
 *
 * 分时计价时界面上显示的是区间内的加权平均价，可能是 1.7325 这种值，
 * 这里去掉多余的尾零，让整数单价仍然显示为 `2` 而不是 `2.0000`。
 */
export function formatPrice(value: number) {
  return String(Number(value.toFixed(4)));
}
