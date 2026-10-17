import type { Model } from "../api";

export function modelProviderName(model: Pick<Model, "group_name" | "base_url">): string {
  const configuredName = String(model.group_name ?? "").trim();
  if (configuredName) return configuredName;

  // 地址缺失时按空串处理：供应商名只用于分组与徽章，不值得为它把整页渲染打崩。
  const value = String(model.base_url ?? "").trim();
  try {
    return new URL(value).hostname.toLowerCase() || value;
  } catch {
    try {
      return new URL(`https://${value}`).hostname.toLowerCase() || value;
    } catch {
      return value;
    }
  }
}
