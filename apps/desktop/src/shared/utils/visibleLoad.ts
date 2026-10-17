/**
 * 可见才取数。
 *
 * 设置页把卡片全部挂在树上（切分区、搜索时草稿与编辑态都不丢），但副作用挡不住：
 * 靠 `hidden` 藏起来的卡片照样会在挂载时各发一轮请求。这个门把"显示"翻译成"取数"，
 * 不可见时既不请求也不回调（返回 `null`，调用方据此跳过应用）。
 */
export function loadWhenVisible<T>(visible: boolean, load: () => Promise<T>): Promise<T | null> {
  if (!visible) return Promise.resolve(null);
  return load();
}
