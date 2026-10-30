const layers: symbol[] = [];

/** 注册一层弹窗；只有最后注册的一层接收 Escape 与 Tab。 */
export function pushModalLayer(layer: symbol) {
  layers.push(layer);
}

/** 关闭或卸载弹窗时移除自己；乱序卸载也不能留下失效的顶层。 */
export function removeModalLayer(layer: symbol) {
  const index = layers.lastIndexOf(layer);
  if (index >= 0) layers.splice(index, 1);
}

export function isTopModalLayer(layer: symbol) {
  return layers.at(-1) === layer;
}
