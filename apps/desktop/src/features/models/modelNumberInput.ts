/**
 * 模型高级字段的数字输入。
 *
 * 这些字段是 `<input type="number">`：正常输入时浏览器只给数字或空串，但粘贴、"e" 记法、
 * 输入法组合过程中的中间态都会给到 `"abc"`、`"1e"`、`"-"` 这类值。原来的写法是
 * `value === "" ? null : Math.trunc(Number(value))`——非数字于是变成 **NaN** 进了草稿：
 * 输入框看着是空的（React 对 NaN 的处理和空串不一样），保存时却报"必须是大于 0 的整数"，
 * 用户根本找不到是哪个字段。非有限值一律当作"没填"。
 */
export function numberFieldValue(value: string): number | null {
  const trimmed = value.trim();
  if (!trimmed) return null;
  const parsed = Number(trimmed);
  // `Number("")` 是 0，`Number("abc")`/`Number("1e")` 是 NaN，`Number("Infinity")` 是 Infinity。
  return Number.isFinite(parsed) ? Math.trunc(parsed) : null;
}
