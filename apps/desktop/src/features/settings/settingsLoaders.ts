import { api, type AppInfo, type OutboundStatus, type ProxySettings, type StatisticsStorage } from "../../shared/api";

/**
 * 设置页三张取数卡的数据读取。
 *
 * 单独放一个 `.ts` 模块（而不是塞在卡片里）：读取是行为，卡片只是画。这样这一层可以在
 * Node 里跑用例——`loadWhenVisible` 的门是不是真的拦住了请求，只有在这里才验得了。
 */

/** 代理设置与出网状态：两个接口各自失败互不牵连（一个读不到不该让另一个也空着）。 */
export async function loadNetworkSettings() {
  const [proxy, outbound] = await Promise.allSettled([api.proxySettings(), api.outboundStatus()]);
  return {
    settings: proxy.status === "fulfilled" ? proxy.value as ProxySettings : null,
    settingsError: proxy.status === "rejected" ? String(proxy.reason) : null,
    outbound: outbound.status === "fulfilled" ? outbound.value as OutboundStatus : null,
    outboundError: outbound.status === "rejected" ? String(outbound.reason) : null,
  };
}

/** 存储占用。 */
export function loadStatisticsStorage(): Promise<StatisticsStorage> {
  return api.statisticsStorage();
}

/** 应用版本、路径与数据目录。 */
export function loadAppInfo(): Promise<AppInfo> {
  return api.appInfo();
}
