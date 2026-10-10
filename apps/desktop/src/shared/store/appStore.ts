import { useSyncExternalStore } from "react";
import { api, ServiceUnreachableError, type CurrencyPricing, type CursorHarnessStatus, type DevinStatus, type LlmCall, type Model, type ModelInput, type Overview, type PluginDescriptor, type PluginRuntimeStatus, type PortSettings, type TokenPricingSettings } from "../api";
import { applyTheme, defaultThemeId, isThemeId, type ThemeId } from "../theme/theme";

/**
 * 首页「价值估算」的默认价格：DeepSeek-V4.1-Flash 官方单价（每百万 token）。
 *
 * 界面语言决定使用哪一套：简体中文用人民币，英文用美元，
 * 两套都是官方公布的原始价格，而不是按汇率折算出来的。
 * 默认走「高峰 / 低谷」模式，低谷价为高峰价的一半；
 * DeepSeek 不单独收取缓存写入费用，因此该项为 0。
 */
const CNY_PRICING: CurrencyPricing = {
  fixed: {
    input_per_million: 2.0,
    output_per_million: 8.0,
    cache_read_per_million: 0.04,
    cache_write_per_million: 0.0,
  },
  peak: {
    input_per_million: 2.0,
    output_per_million: 8.0,
    cache_read_per_million: 0.04,
    cache_write_per_million: 0.0,
  },
  off_peak: {
    input_per_million: 1.0,
    output_per_million: 4.0,
    cache_read_per_million: 0.02,
    cache_write_per_million: 0.0,
  },
};

const USD_PRICING: CurrencyPricing = {
  fixed: {
    input_per_million: 0.3,
    output_per_million: 1.2,
    cache_read_per_million: 0.006,
    cache_write_per_million: 0.0,
  },
  peak: {
    input_per_million: 0.3,
    output_per_million: 1.2,
    cache_read_per_million: 0.006,
    cache_write_per_million: 0.0,
  },
  off_peak: {
    input_per_million: 0.15,
    output_per_million: 0.6,
    cache_read_per_million: 0.003,
    cache_write_per_million: 0.0,
  },
};

export const DEFAULT_TOKEN_PRICING: TokenPricingSettings = {
  mode: "peak_off_peak",
  cny: CNY_PRICING,
  usd: USD_PRICING,
};

export type AppSnapshot = {
  models: Model[];
  calls: LlmCall[];
  overview: Overview;
  detailed: boolean;
  /** 开发者模式：界面显示内部标识与协议字段。默认关，存在 server 的 desktop 设置里。 */
  developerMode: boolean;
  ports: PortSettings;
  pricing: TokenPricingSettings;
  busy: boolean;
  error: string | null;
  /** 本地管理服务连不上时为 true；界面据此持续重试并明确说明状态。 */
  offline: boolean;
  theme: ThemeId;
  cursorHarness: CursorHarnessStatus | null;
  /** Gateway state for the Devin module, refreshed with everything else. */
  devinStatus: DevinStatus | null;
  cursorBusy: boolean;
  pluginRuntime: PluginRuntimeStatus | null;
  plugins: PluginDescriptor[];
};

const savedTheme = (): ThemeId => {
  const saved = localStorage.getItem("haxsd-byok.theme");
  return isThemeId(saved) ? saved : defaultThemeId;
};

let snapshot: AppSnapshot = {
  models: [],
  calls: [],
  overview: {
    metrics: {
      llm_calls: 0,
      successful_calls: 0,
      failed_calls: 0,
      token_usage: 0,
      prompt_tokens: 0,
      input_tokens: 0,
      cache_read_tokens: 0,
      cache_write_tokens: 0,
      output_tokens: 0,
    },
    token_usage_granularity: "day",
    token_usage_series: [],
  },
  detailed: false,
  developerMode: false,
  ports: { proxy_port: 0, service_port: 0 },
  pricing: DEFAULT_TOKEN_PRICING,
  busy: false,
  error: null,
  offline: false,
  theme: savedTheme(),
  cursorHarness: null,
  devinStatus: null,
  cursorBusy: false,
  pluginRuntime: null,
  plugins: [],
};

const listeners = new Set<() => void>();

function update(patch: Partial<AppSnapshot>) {
  snapshot = { ...snapshot, ...patch };
  listeners.forEach((listener) => listener());
}

async function perform(task: () => Promise<void>) {
  update({ error: null });
  try {
    await task();
    if (snapshot.offline) update({ offline: false });
  } catch (cause) {
    if (cause instanceof ServiceUnreachableError) update({ offline: true, error: null });
    else update({ error: cause instanceof Error ? cause.message : String(cause) });
  }
}

/**
 * 刷新代际。挂载时的刷新、手动点刷新、改完设置后的刷新会重叠，没有这个守卫时
 * 先发起的请求晚返回就会把新数据覆盖回旧值（"刚改完又显示旧的"）。
 */
let refreshGeneration = 0;

/** 当前在跑的刷新。发起新刷新或做数据变更前先中止它，避免旧结果回填。 */
let refreshController: AbortController | null = null;

/**
 * 作废在途的刷新：只递增代际并中止它的请求，不发起新的一次。
 *
 * 变更类操作在动手前先调用，保证旧刷新晚返回时不会把刚写入的状态覆盖回去；
 * 之后再按需调用 refresh() 拉取权威数据。
 */
function invalidateRefresh() {
  refreshGeneration += 1;
  refreshController?.abort();
  refreshController = null;
}

export const appStore = {
  subscribe(listener: () => void) {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
  getSnapshot: () => snapshot,

  /**
   * 十一个接口分别落库：一个次要接口（比如插件运行时）失败时，其余数据照常更新，
   * 只有失败项保留旧值。以前整批走 `Promise.all`，任一接口抛错就把已经拿到的数据
   * 全部丢掉，界面上表现为"数据停在刚才"。
   */
  async refresh() {
    const generation = ++refreshGeneration;
    // 中止上一次还没落库的刷新，避免它晚返回时把新数据覆盖回旧值。
    refreshController?.abort();
    const controller = new AbortController();
    refreshController = controller;
    const { signal } = controller;
    update({ busy: true, error: null });
    const results = await Promise.allSettled([
      api.models(signal),
      api.calls(signal),
      api.overview(undefined, signal),
      api.observability(signal),
      api.ports(signal),
      api.pricingSettings(signal),
      api.cursorHarness(signal),
      api.devinStatus(signal),
      api.pluginRuntime(signal),
      api.plugins(signal),
      api.desktopSettings(signal),
    ]);
    // 已经有更新的一批在跑：这一批整批丢弃，busy 交给新的一批收尾。
    // 若是变更操作只作废、没有新开刷新（refreshController 已清空），这里要自己把 busy 清掉，
    // 否则界面会一直转圈。
    if (generation !== refreshGeneration) {
      if (refreshController === null) update({ busy: false });
      return;
    }
    if (refreshController === controller) refreshController = null;
    const labels = [
      t("模型库"), t("调用记录"), t("概览统计"), t("调用观测"), t("端口设置"),
      t("Token 定价"), t("Cursor 接管状态"), t("Devin 网关状态"), t("插件运行时"),
      t("插件列表"), t("应用设置"),
    ];
    const [models, calls, overview, observability, ports, pricing, cursorHarness, devinStatus, pluginRuntime, plugins, desktop] = results;
    const patch: Partial<AppSnapshot> = {};
    if (models.status === "fulfilled") patch.models = models.value;
    if (calls.status === "fulfilled") patch.calls = calls.value;
    if (overview.status === "fulfilled") patch.overview = overview.value;
    if (observability.status === "fulfilled") patch.detailed = observability.value.detailed;
    if (ports.status === "fulfilled") patch.ports = ports.value;
    if (pricing.status === "fulfilled") patch.pricing = pricing.value;
    if (cursorHarness.status === "fulfilled") patch.cursorHarness = cursorHarness.value;
    if (devinStatus.status === "fulfilled") patch.devinStatus = devinStatus.value;
    if (pluginRuntime.status === "fulfilled") patch.pluginRuntime = pluginRuntime.value;
    if (plugins.status === "fulfilled") patch.plugins = plugins.value;
    if (desktop.status === "fulfilled") patch.developerMode = desktop.value.developer_mode;
    const failures = results.filter((result): result is PromiseRejectedResult => result.status === "rejected");
    // 只有「服务连不上」才算离线；超时是 RequestTimeoutError，不算离线。
    const unreachable = failures.some((failure) => failure.reason instanceof ServiceUnreachableError);
    // offline 的语义不变：只要有接口报"连不上"就是离线；只要有接口答了话，就不算离线。
    if (unreachable) patch.offline = true;
    else if (failures.length < results.length) patch.offline = false;
    const failedLabels = results.flatMap((result, index) => result.status === "rejected" && !(result.reason instanceof ServiceUnreachableError) ? [labels[index]] : []);
    if (failedLabels.length > 0) {
      patch.error = t("部分数据刷新失败：{items}。其余数据已更新，失败项保留上一次的值。", { items: failedLabels.join("、") });
    }
    update(patch);
    update({ busy: false });
  },

  async deleteModel(modelHash: string) {
    await perform(async () => {
      // 变更前先作废在途刷新，并乐观移除，界面立刻反映删除。
      invalidateRefresh();
      update({ models: snapshot.models.filter((model) => model.model_hash !== modelHash) });
      await api.deleteModel(modelHash);
      await appStore.refresh();
    });
  },

  async initializeCursorCa() {
    update({ cursorBusy: true, error: null });
    try {
      const status = await api.initializeCursorCa();
      update({ cursorHarness: status });
      return status;
    } catch (cause) {
      update({ error: cause instanceof Error ? cause.message : String(cause) });
      return null;
    } finally { update({ cursorBusy: false }); }
  },
  async initializePluginRuntime() {
    update({ error: null });
    try {
      const pluginRuntime = await api.initializePluginRuntime();
      update({ pluginRuntime });
      return pluginRuntime;
    } catch (cause) {
      update({ error: cause instanceof Error ? cause.message : String(cause) });
      return null;
    }
  },
  async refreshPluginRuntime() {
    try {
      const wasReady = snapshot.pluginRuntime?.state === "ready";
      const pluginRuntime = await api.pluginRuntime();
      update({ pluginRuntime });
      if (!wasReady && pluginRuntime.state === "ready") {
        const plugins = await api.plugins();
        update({ plugins });
      }
      return pluginRuntime;
    } catch (cause) {
      update({ error: cause instanceof Error ? cause.message : String(cause) });
      return null;
    }
  },
  async cancelPluginRuntimeInitialization() {
    try {
      const pluginRuntime = await api.cancelPluginRuntimeInitialization();
      update({ pluginRuntime });
      return pluginRuntime;
    } catch (cause) {
      update({ error: cause instanceof Error ? cause.message : String(cause) });
      return null;
    }
  },
  async refreshPlugins() {
    try {
      update({ plugins: await api.plugins() });
    } catch (cause) {
      update({ error: cause instanceof Error ? cause.message : String(cause) });
    }
  },
  async removePluginConfiguration(pluginId: string) {
    await perform(async () => {
      await api.removePluginConfiguration(pluginId);
      update({ plugins: await api.plugins() });
    });
  },
  async setCursorEnabled(enabled: boolean) {
    update({ cursorBusy: true, error: null });
    try { update({ cursorHarness: await api.setCursorEnabled(enabled) }); }
    catch (cause) { update({ error: cause instanceof Error ? cause.message : String(cause) }); }
    finally { update({ cursorBusy: false }); }
  },
  async createModels(models: ModelInput[]) {
    update({ cursorBusy: true, error: null });
    try {
      // 变更前先作废在途刷新：否则旧 GET 晚返回会盖掉刚建的模型。
      invalidateRefresh();
      const created = await api.createModels(models);
      // 先把新建结果并进快照，界面不必等刷新完成就能看到变化。
      if (created.length > 0) {
        const byHash = new Map(snapshot.models.map((model) => [model.model_hash, model] as const));
        for (const model of created) byHash.set(model.model_hash, model);
        update({ models: [...byHash.values()].sort((a, b) => a.sort_order - b.sort_order) });
      }
      await appStore.refresh();
      return created;
    } catch (cause) {
      update({ error: cause instanceof Error ? cause.message : String(cause) });
      return null;
    } finally { update({ cursorBusy: false }); }
  },
  /** 复制模型连同它的凭据：凭据不回传前端，所以由服务端从源模型读回。 */
  async duplicateModel(hash: string, displayName: string) {
    update({ cursorBusy: true, error: null });
    try {
      invalidateRefresh();
      const created = await api.duplicateModel(hash, displayName);
      if (created) {
        const byHash = new Map(snapshot.models.map((model) => [model.model_hash, model] as const));
        byHash.set(created.model_hash, created);
        update({ models: [...byHash.values()].sort((a, b) => a.sort_order - b.sort_order) });
      }
      await appStore.refresh();
      return created;
    } catch (cause) {
      update({ error: cause instanceof Error ? cause.message : String(cause) });
      return null;
    } finally { update({ cursorBusy: false }); }
  },
  async importV0049Models() {
    update({ cursorBusy: true, error: null });
    try {
      invalidateRefresh();
      const result = await api.importV0049Models();
      await appStore.refresh();
      return result;
    } catch (cause) {
      update({ error: cause instanceof Error ? cause.message : String(cause) });
      return null;
    } finally { update({ cursorBusy: false }); }
  },
  async updateCursorModel(hash: string, model: ModelInput) {
    update({ cursorBusy: true, error: null });
    try {
      invalidateRefresh();
      const updated = await api.updateModel(hash, model);
      // 立刻反映这一条模型的改动，其余字段交给紧随其后的刷新。
      if (updated) {
        update({ models: snapshot.models.map((item) => item.model_hash === updated.model_hash ? updated : item) });
      }
      await appStore.refresh();
      return updated;
    } catch (cause) {
      update({ error: cause instanceof Error ? cause.message : String(cause) });
      return null;
    } finally { update({ cursorBusy: false }); }
  },
  async reorderCursorModels(modelHashes: string[]) {
    // 作废在途刷新，避免它晚返回时用旧顺序覆盖这次重排。
    invalidateRefresh();
    const previous = snapshot.models;
    const byHash = new Map(previous.map((model) => [model.model_hash, model]));
    if (modelHashes.length !== previous.length || new Set(modelHashes).size !== previous.length) {
      update({ error: t("模型配置已发生变化，请刷新后重试") });
      return false;
    }
    const reordered: Model[] = [];
    for (const [index, hash] of modelHashes.entries()) {
      const model = byHash.get(hash);
      if (!model) {
        update({ error: t("模型配置已发生变化，请刷新后重试") });
        return false;
      }
      reordered.push({ ...model, sort_order: index + 1 });
    }
    update({ models: reordered, cursorBusy: true, error: null });
    try {
      update({ models: await api.reorderModels(modelHashes) });
      return true;
    } catch (cause) {
      update({
        models: previous,
        error: cause instanceof Error ? cause.message : String(cause),
      });
      return false;
    } finally {
      update({ cursorBusy: false });
    }
  },

  async refreshCalls() {
    try {
      update({ calls: await api.calls(), offline: false });
    } catch (cause) {
      if (cause instanceof ServiceUnreachableError) update({ offline: true });
      else update({ error: cause instanceof Error ? cause.message : String(cause) });
    }
  },

  async openCallDetails(callId: string) {
    await perform(() => api.openCallDetails(callId));
  },
  async updateDetailed(detailed: boolean) {
    await perform(async () => update(await api.setObservability(detailed)));
  },
  // 开发者模式：desktop 设置是一整份，先读回再合并，避免覆盖静默启动等字段。
  async updateDeveloperMode(enabled: boolean) {
    update({ error: null });
    try {
      const settings = await api.desktopSettings();
      const saved = await api.setDesktopSettings({ ...settings, developer_mode: enabled });
      update({ developerMode: saved.developer_mode });
      return true;
    } catch (cause) {
      update({ error: cause instanceof Error ? cause.message : String(cause) });
      return false;
    }
  },
  async updatePorts(ports: PortSettings) {
    try {
      update({ error: null });
      update({ ports: await api.setPorts(ports) });
      return true;
    } catch (cause) {
      update({ error: cause instanceof Error ? cause.message : String(cause) });
      return false;
    }
  },
  async updatePricingSettings(pricing: TokenPricingSettings) {
    try {
      update({ error: null });
      update({ pricing: await api.setPricingSettings(pricing) });
      return true;
    } catch (cause) {
      update({ error: cause instanceof Error ? cause.message : String(cause) });
      return false;
    }
  },
  selectTheme(theme: ThemeId) {
    localStorage.setItem("haxsd-byok.theme", theme);
    applyTheme(theme);
    update({ theme });
  },
};

export function useAppStore() {
  return useSyncExternalStore(appStore.subscribe, appStore.getSnapshot);
}
