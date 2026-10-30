/** 端口输入只产生 0..65535 的整数；0 交给服务端按统一文案拒绝。 */
export function portInputValue(raw: string): number {
  const value = Number(raw);
  if (!Number.isFinite(value)) return 0;
  return Math.min(65_535, Math.max(0, Math.trunc(value)));
}

/** 可选正整数：空值或无法解析时清空，负数夹到 1，避免 JSON 里出现 NaN/null 混淆。 */
export function optionalPositiveInteger(raw: string): number | null {
  if (!raw.trim()) return null;
  const value = Number(raw);
  if (!Number.isFinite(value)) return null;
  return Math.max(1, Math.trunc(value));
}
