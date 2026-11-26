import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, type DevinHostStatus, type DevinModelBinding, type DevinRoute, type DevinSettings } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { Card } from "../../shared/ui/Card";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { ModelSelect } from "../../shared/ui/ModelSelect";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import { StatusHero } from "../../shared/ui/StatusHero";
import { PageContent } from "../../shell/layout/PageContent";
import { PageTitle } from "../../shared/ui/PageTitle";
import { StatusPill } from "../../shared/ui/StatusPill";
import { clientErrorText, writeFailureText } from "../harness/harnessErrorText";
import { devinConnectionView, devinHostRepairable, devinPointsAtGateway } from "./devinConnectionView";
import { applyDevinHostConfiguration, canRestoreDevinHost } from "./devinHostApply";
import {
  appendRouteForBinding,
  isNativeModelUid,
  keepActiveRouteId,
  nativeBindingForNextModel,
} from "./devinBindingEdits";
import { devinDefaultModelOptions, devinModelOptions } from "./devinModelOptions";
import { appStore, useAppStoreSelector } from "../../shared/store/appStore";
import { getUiPreferences, saveUiPreferences, useUiPreferences } from "../../shared/store/uiPreferences";
import { portInputValue } from "../../shared/utils/portInput";
import { probeHostPath } from "./hostPathProbe";
import { HarnessCapabilities } from "../harness/HarnessCapabilities";
import styles from "./DevinSettingsPage.module.scss";

const emptySettings: DevinSettings = {
  enabled: false,
  api_port: 43_110,
  inference_port: 43_111,
  local_api_port: 43_112,
  upstream_api_url: "https://server.codeium.com",
  bindings: [],
  default_model_uid: null,
};
/** 旧设置只有主模型哈希，界面把它显示成一条合成的主路由，直到用户真正编辑绑定。 */
const legacyRouteId = "primary";

/** Devin 设置文件路径必须记住：状态面板、检查、写入全靠它，刷新即丢会让人以为功能坏了。 */
/** 存在服务端（原来在 localStorage：端口一变路径就没了），见 shared/store/uiPreferences.ts。 */
function readHostPath(): string {
  return getUiPreferences().devinHostPath ?? "";
}

/** 记住宿主路径；写失败不影响当前会话。 */
function writeHostPath(path: string) {
  void saveUiPreferences({ devinHostPath: path.trim() || null });
}

/** Devin 装在自定义位置时记住安装目录；空值表示交给服务端自动探测。 */
function readHostInstallRoot(): string {
  return getUiPreferences().devinInstallRoot ?? "";
}

function writeHostInstallRoot(root: string) {
  void saveUiPreferences({ devinInstallRoot: root.trim() || null });
}

function routesForDisplay(binding: DevinModelBinding): DevinRoute[] {
  if (binding.routes.length) return binding.routes;
  return [{ route_id: legacyRouteId, model_hash: binding.model_hash, label: binding.display_name, enabled: true }];
}

function activeRouteIdForDisplay(binding: DevinModelBinding): string | null {
  return binding.routes.length ? binding.active_route_id : legacyRouteId;
}

function nextRouteId(routes: DevinRoute[]): string {
  const used = new Set(routes.map((route) => route.route_id));
  let index = routes.length + 1;
  while (used.has(`route-${index}`)) index += 1;
  return `route-${index}`;
}

/**
 * 一行映射在无障碍名称里的身份：Devin 侧的模型标识最能说明"这一行是哪一条"，
 * 没填标识时退回显示名称或哈希。同一页上「更多」「移除」这类按钮会重复出现多次，
 * 不带上身份就只能听到一串一样的按钮名。
 */
function bindingName(binding: DevinModelBinding): string {
  const uid = String(binding.model_uid ?? "").trim();
  return (isNativeModelUid(uid) ? binding.display_name.trim() : uid)
    || binding.display_name
    || binding.model_hash
    || "";
}

/** 一条候选路由的身份：路由名优先，其次路由本身。 */
function routeName(route: DevinRoute): string {
  return String(route.label ?? "").trim() || route.route_id;
}

export function DevinSettingsPage() {
  const message = useMessage();
  const navigate = useNavigate();
  const [settings, setSettings] = useState<DevinSettings>(emptySettings);
  // The last persisted snapshot, so the page can say whether anything is pending.
  // The save button used to live inside the collapsed section, which meant a change
  // made in the open one looked applied while nothing had been written.
  const [saved, setSaved] = useState<DevinSettings>(emptySettings);
  const models = useAppStoreSelector((state) => state.models);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [hostPath, setHostPath] = useState(readHostPath);
  const [hostInstallRoot, setHostInstallRoot] = useState(readHostInstallRoot);
  const hostPathRef = useRef(hostPath);
  const hostInstallRootRef = useRef(hostInstallRoot);
  hostPathRef.current = hostPath;
  hostInstallRootRef.current = hostInstallRoot;
  const [hostStatus, setHostStatus] = useState<DevinHostStatus | null>(null);
  const [hostFailure, setHostFailure] = useState<string | null>(null);
  const [hostBusy, setHostBusy] = useState(false);
  const [repairingHost, setRepairingHost] = useState(false);
  const [hostNeedsInspect, setHostNeedsInspect] = useState(false);
  const [confirmForceHost, setConfirmForceHost] = useState(false);
  const hostPathSaveTimer = useRef<number | null>(null);
  const hostInstallRootSaveTimer = useRef<number | null>(null);
  // Which mapping rows are showing their optional fields. Collapsed by default: the
  // page should read as two decisions per mapping, not as a form of five fields.
  const [expandedBindings, setExpandedBindings] = useState<Record<string, boolean>>({});
  const devinStatus = useAppStoreSelector((state) => state.devinStatus);

  const persistHostPath = (value: string) => {
    if (hostPathSaveTimer.current !== null) {
      window.clearTimeout(hostPathSaveTimer.current);
      hostPathSaveTimer.current = null;
    }
    writeHostPath(value);
  };
  const persistHostInstallRoot = (value: string) => {
    if (hostInstallRootSaveTimer.current !== null) {
      window.clearTimeout(hostInstallRootSaveTimer.current);
      hostInstallRootSaveTimer.current = null;
    }
    writeHostInstallRoot(value);
  };
  const flushHostPath = (value: string) => {
    if (hostPathSaveTimer.current === null) return;
    persistHostPath(value);
  };
  const flushHostInstallRoot = (value: string) => {
    if (hostInstallRootSaveTimer.current === null) return;
    persistHostInstallRoot(value);
  };

  // 偏好是启动时异步读回来的：值到了之后补上宿主路径（用户没在这期间自己填过才算）。
  const rememberedHostPath = useUiPreferences().devinHostPath ?? "";
  useEffect(() => {
    if (rememberedHostPath) setHostPath((current) => current || rememberedHostPath);
  }, [rememberedHostPath]);
  const rememberedInstallRoot = useUiPreferences().devinInstallRoot ?? "";
  useEffect(() => {
    if (rememberedInstallRoot) setHostInstallRoot((current) => current || rememberedInstallRoot);
  }, [rememberedInstallRoot]);

  useEffect(() => {
    let disposed = false;
    setLoading(true);
    setLoadError(null);
    void api.devinSettings().then((value) => {
      if (disposed) return;
      setSettings(value);
      setSaved(value);
    }).catch((cause) => {
      if (disposed) return;
      const text = clientErrorText(cause);
      setLoadError(text);
      message.error(text);
    }).finally(() => {
      if (!disposed) setLoading(false);
    });
    return () => { disposed = true; };
  }, [message, reloadKey]);

  useEffect(() => () => {
    if (hostPathSaveTimer.current !== null) window.clearTimeout(hostPathSaveTimer.current);
    if (hostInstallRootSaveTimer.current !== null) window.clearTimeout(hostInstallRootSaveTimer.current);
  }, []);

  useEffect(() => {
    if (loading || loadError) return;
    void appStore.refreshDevinStatus();
  }, [loading, loadError]);

  // 设置文件路径与安装目录不再要求用户填写：服务端自己找到它们，再回填到界面。
  useEffect(() => {
    if (loading || loadError) return;
    let cancelled = false;
    const probedPath = hostPathRef.current;
    const probedRoot = hostInstallRootRef.current;
    void probeHostPath(probedPath, api.devinHostStatus, probedRoot)
      .then((probe) => {
        if (cancelled) return;
        setHostStatus(probe.status);
        setHostFailure(probe.failure);
        setHostNeedsInspect(false);
        if (probe.status && probe.status.path !== probedPath.trim() && hostPathRef.current === probedPath) {
          setHostPath(probe.status.path);
          persistHostPath(probe.status.path);
        }
        if (probe.status && probedRoot.trim() && !probe.matched.installRoot && hostInstallRootRef.current === probedRoot) {
          setHostInstallRoot("");
          persistHostInstallRoot("");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [loading, loadError]);

  const rememberHostPath = (value: string) => {
    setHostPath(value);
    setHostStatus(null);
    setHostFailure(null);
    setHostNeedsInspect(true);
    if (hostPathSaveTimer.current !== null) window.clearTimeout(hostPathSaveTimer.current);
    hostPathSaveTimer.current = window.setTimeout(() => {
      hostPathSaveTimer.current = null;
      writeHostPath(value);
    }, 400);
  };
  const rememberHostInstallRoot = (value: string) => {
    setHostInstallRoot(value);
    setHostStatus(null);
    setHostFailure(null);
    setHostNeedsInspect(true);
    if (hostInstallRootSaveTimer.current !== null) window.clearTimeout(hostInstallRootSaveTimer.current);
    hostInstallRootSaveTimer.current = window.setTimeout(() => {
      hostInstallRootSaveTimer.current = null;
      writeHostInstallRoot(value);
    }, 400);
  };

  const update = <K extends keyof DevinSettings>(key: K, value: DevinSettings[K]) => {
    setSettings((current) => ({ ...current, [key]: value }));
  };
  const updateBinding = (index: number, patch: Partial<DevinModelBinding>) => {
    setSettings((current) => {
      const binding = current.bindings[index];
      let defaultModelUid = current.default_model_uid;
      if (binding && defaultModelUid?.trim() === binding.model_uid.trim()) {
        const nextUid = typeof patch.model_uid === "string"
          ? patch.model_uid.trim()
          : binding.model_uid.trim();
        const nextEnabled = patch.enabled ?? binding.enabled;
        defaultModelUid = nextEnabled && nextUid ? nextUid : null;
      }
      return {
        ...current,
        default_model_uid: defaultModelUid,
        bindings: current.bindings.map((item, itemIndex) => itemIndex === index ? { ...item, ...patch } : item),
      };
    });
  };
  const addNativeBinding = () => {
    setSettings((current) => {
      const binding = nativeBindingForNextModel(current.bindings, models);
      if (!binding) return current;
      return {
        ...current,
        enabled: true,
        default_model_uid: current.bindings.length === 0 ? binding.model_uid : current.default_model_uid,
        bindings: [...current.bindings, binding],
      };
    });
  };
  const removeBinding = (index: number) => {
    setSettings((current) => {
      const removed = current.bindings[index];
      return {
        ...current,
        default_model_uid: removed && current.default_model_uid?.trim() === removed.model_uid.trim()
          ? null
          : current.default_model_uid,
        bindings: current.bindings.filter((_, itemIndex) => itemIndex !== index),
      };
    });
  };
  const commitRoutes = (index: number, routes: DevinRoute[], activeRouteId: string | null) => {
    updateBinding(index, { routes, active_route_id: keepActiveRouteId(routes, activeRouteId) });
  };
  const updateRoute = (index: number, binding: DevinModelBinding, routes: DevinRoute[], routeIndex: number, patch: Partial<DevinRoute>) => {
    commitRoutes(index, routes.map((route, itemIndex) => itemIndex === routeIndex ? { ...route, ...patch } : route), activeRouteIdForDisplay(binding));
  };
  const appendRoute = (index: number, binding: DevinModelBinding, routes: DevinRoute[]) => {
    const model = models[0];
    updateBinding(index, appendRouteForBinding(binding, {
      route_id: nextRouteId(routes),
      model_hash: model?.model_hash ?? binding.model_hash,
      label: model?.display_name ?? "",
      enabled: true,
    }));
  };
  const removeRoute = (index: number, binding: DevinModelBinding, routes: DevinRoute[], routeIndex: number) => {
    commitRoutes(index, routes.filter((_, itemIndex) => itemIndex !== routeIndex), activeRouteIdForDisplay(binding));
  };
  const inspectHost = async () => {
    try {
      setHostBusy(true);
      const status = await api.devinHostStatus(hostPath, hostInstallRoot);
      setHostStatus(status);
      setHostFailure(null);
      if (status.path !== hostPath.trim()) {
        setHostPath(status.path);
        persistHostPath(status.path);
      }
    } catch (cause) {
      // 用户主动点"检查设置"，路径无效就要立刻说清楚（说法由界面给，不显示服务端英文原句）。
      message.error(clientErrorText(cause));
      setHostFailure(clientErrorText(cause));
      setHostStatus(null);
    } finally {
      setHostBusy(false);
      setHostNeedsInspect(false);
    }
  };
  const applyHostConfiguration = async (force = false) => {
    setHostBusy(true);
    try {
      const outcome = await applyDevinHostConfiguration(hostPath, hostInstallRoot, {}, force);
      if (outcome.kind === "write-failed") {
        // 设置没写下去：文件保持原样，"写入设置"仍然可用。
        const text = writeFailureText(outcome.cause);
        message.error(text);
        setHostFailure(text);
        return;
      }
      if (outcome.kind === "status-unreadable") {
        // 设置已经写进文件，是读状态失败。这里不能说"写入失败"——那会诱导用户再点一次。
        // 清掉旧状态并指向「检查设置」。
        setHostStatus(null);
        const text = t("设置已写入，但状态没能读回来：点「检查设置」确认结果，不要重复写入。");
        setHostFailure(text);
        message.error(text, { duration: 8_000 });
        return;
      }
      setHostStatus(outcome.status);
      await appStore.refreshDevinStatus();
      setHostFailure(null);
      if (outcome.status.path !== hostPath.trim()) {
        setHostPath(outcome.status.path);
        persistHostPath(outcome.status.path);
      }
      // 成功消息只在状态读回来之后才发：设置落盘和"界面知道设置落盘了"是两件事。
      message(t("Devin 接入配置已写入；重新打开 Devin 后生效"), { duration: 5_000 });
    } finally {
      setHostBusy(false);
      setHostNeedsInspect(false);
    }
  };
  const repairHostConfiguration = async () => {
    if (!hostStatus?.extension.path) return;
    setHostBusy(true);
    setRepairingHost(true);
    try {
      const status = await api.repairDevinHostConfiguration(hostStatus.path, hostStatus.extension.path);
      setHostStatus(status);
      await appStore.refreshDevinStatus();
      setHostFailure(null);
      if (status.path !== hostPath.trim()) {
        setHostPath(status.path);
        persistHostPath(status.path);
      }
      message(t("Devin 接管补丁已修复；重新打开 Devin 后生效"), { duration: 5_000 });
    } catch (cause) {
      const text = clientErrorText(cause);
      message.error(text, { duration: 8_000 });
      setHostFailure(text);
    } finally {
      setHostBusy(false);
      setRepairingHost(false);
      setHostNeedsInspect(false);
    }
  };
  const restoreHostConfiguration = async () => {    if (!hostStatus || !canRestoreDevinHost(hostStatus)) return;
    try {
      setHostBusy(true);
      await api.restoreDevinHostConfiguration(hostStatus.path, hostInstallRoot);
      const status = await api.devinHostStatus(hostStatus.path, hostInstallRoot);
      setHostStatus(status);
      await appStore.refreshDevinStatus();
      setHostFailure(null);
      if (status.path !== hostPath.trim()) {
        setHostPath(status.path);
        persistHostPath(status.path);
      }
      message(t("Devin 接入配置已恢复"));
    } catch (cause) {
      message.error(writeFailureText(cause));
    } finally {
      setHostBusy(false);
      setHostNeedsInspect(false);
    }
  };
  const persistSettings = async (notify: boolean) => {
    try {
      setSaving(true);
      const next = await api.setDevinSettings({ ...settings, enabled: true });
      setSettings(next);
      setSaved(next);
      await appStore.refreshDevinStatus();
      if (notify) message(t("设置已保存。"), { duration: 3_000 });
      return next;
    } catch (cause) {
      message.error(clientErrorText(cause));
      return null;
    } finally {
      setSaving(false);
    }
  };
  const save = () => void persistSettings(true);
  // 接入状态只看已保存的配置与服务端报告的监听状态：草稿上的改动只让底部出现
  // "有未保存的改动"，不能让状态面板声称某个开关已经生效。
  const view = devinConnectionView({
    draft: settings,
    saved,
    models,
    hostStatus,
    gateway: devinStatus ? { listening: devinStatus.listening, ready: devinStatus.ready } : null,
    devinCalls: devinStatus?.calls ?? null,
    lastCallSuccess: devinStatus?.last_call_success ?? null,
  });

  const modelOptions = devinModelOptions(models);
  const defaultModelOptions = devinDefaultModelOptions(settings.bindings, models);
  const defaultBinding = settings.bindings.find((binding) => (
    binding.enabled
    && binding.model_uid.trim() === settings.default_model_uid?.trim()
  )) ?? null;
  const defaultMissingReason = settings.enabled && !defaultBinding
    ? settings.default_model_uid
      ? t("当前默认模型已停用或不可用，请重新选择。")
      : t("请选择一个默认模型，保存后才能使用。")
    : null;
  const canAddNativeBinding = nativeBindingForNextModel(settings.bindings, models) !== null;
  const hostIntegrated = devinPointsAtGateway(hostStatus, saved);
  const hostRestorable = canRestoreDevinHost(hostStatus);
  const extensionMissing = hostStatus != null && hostStatus.extension.path == null;
  const extensionUnsupported = hostStatus != null && hostStatus.extension.path != null && !hostStatus.extension.compatible;
  const canAutoConnect = Boolean(hostStatus?.unconfigured && hostStatus.extension.compatible && hostStatus.extension.clean);
  const hostRepairable = saved.enabled
    && !hostIntegrated
    && devinHostRepairable(hostStatus, {
      api_port: saved.api_port,
      inference_port: saved.inference_port,
      local_api_port: saved.local_api_port,
    });
  const connectDisabled = loading
    || Boolean(loadError)
    || saving
    || hostBusy
    || hostNeedsInspect
    || !saved.enabled
    || !saved.bindings.some(binding => binding.enabled
      && binding.model_uid.trim() === saved.default_model_uid?.trim())
    || view.pendingChanges
    || !canAutoConnect;
  const hostPrimaryText = hostFailure
    ? clientErrorText(hostFailure)
    : hostNeedsInspect
      ? t("路径已修改。点“重新检查”确认新的位置。")
      : !hostStatus
        ? t("正在检查 Devin 配置…")
        : hostIntegrated
          ? t("已连接。重新打开 Devin 就可以对话。")
          : hostRepairable
            ? t("Devin 更新后接管补丁未生效。关闭 Devin，然后点上方“修复接管”。")
            : canAutoConnect
            ? view.pendingChanges
              ? t("先保存设置，再点上方主按钮“应用到 Devin”。")
              : defaultMissingReason
                ?? t("关闭 Devin，然后点上方主按钮“应用到 Devin”。")
            : extensionMissing
              ? t("找不到 Devin。请先安装或打开一次 Devin；如果装在自定义位置，请在“高级”里填写安装目录。")
              : extensionUnsupported
                ? hostStatus.extension.patched && hostRestorable
                  ? t("接管配置需要恢复。点上方“撤销接管”，再重新应用。")
                  : t("当前 Devin 版本暂不支持自动接入。")
                : hostStatus.configured
                  ? hostRestorable
                    ? t("接管配置需要恢复。点上方“撤销接管”，再重新应用。")
                    : t("Devin 配置存在冲突，请查看下面的技术详情。")
                  : t("暂时不能自动接入，请查看下面的技术详情。");
  const primaryAction = (() => {
    if (loading || loadError) return { label: t("读取中…"), disabled: true, run: () => undefined };
    if (!models.length) return { label: t("管理模型"), disabled: false, run: () => void navigate("/models") };
    if (view.pendingChanges) {
      return { label: t("保存"), disabled: saving || Boolean(defaultMissingReason), run: save };
    }
    if (!saved.enabled || !saved.bindings.some(binding => binding.enabled
      && binding.model_uid.trim() === saved.default_model_uid?.trim())) {
      return { label: t("保存"), disabled: true, run: save };
    }
    if (hostRepairable) {
      return { label: repairingHost ? t("修复中…") : t("修复接管"), disabled: saving || hostBusy, run: () => void repairHostConfiguration() };
    }
    if (hostIntegrated) {
      return { label: t("查看调用记录"), disabled: false, run: () => void navigate("/calls") };
    }
    if (canAutoConnect) {
      return { label: t("应用到 Devin"), disabled: connectDisabled, run: () => void applyHostConfiguration() };
    }
    return { label: t("重新检查"), disabled: hostBusy, run: () => void inspectHost() };
  })();
  const showHostCard = !hostIntegrated
    || Boolean(hostFailure)
    || hostNeedsInspect
    || hostRepairable
    || extensionUnsupported
    || Boolean(hostStatus && !hostStatus.unconfigured && !hostStatus.configured && hostStatus.message);
  const content = loading
    ? <div className={styles.loading}>{t("加载中…")}</div>
    : loadError
      ? <div className={styles.loadError} role="alert"><span>{loadError}</span><Button size="small" onClick={() => setReloadKey(key => key + 1)}>{t("重试")}</Button></div>
      : <div className={styles.page}>
    <Card as="section">
      <StatusHero
        connected={view.hero.connected}
        title={view.hero.title}
        description={view.hero.description}
        stages={view.hero.stages}
        steps={[]}
        action={<>
          {hostStatus?.configured && !hostIntegrated && !hostStatus.extension.patched && <Button size="small" disabled={saving || hostBusy} onClick={() => setConfirmForceHost(true)}>{t("强制接管")}</Button>}
          {hostRestorable && <Button size="small" disabled={saving || hostBusy || hostNeedsInspect} onClick={() => void restoreHostConfiguration()}>{t("撤销接管")}</Button>}
          <Button size="small" variant="primary" disabled={primaryAction.disabled} onClick={primaryAction.run}>{primaryAction.label}</Button>
        </>}
      />
    </Card>
    <TitledCard
      title={t("使用哪个模型")}
      description={t("Devin 的对话会由你选择的模型回答。")}
      collapsible
      storageKey="devin-models"
      badge={<StatusPill tone="idle">{t("{count} 个模型", { count: settings.bindings.length })}</StatusPill>}
    >
      <div className={styles.settingRow}>
        <div><strong>{t("对话模型")}</strong><small>{t("直接选择模型库里的模型；它会以原名出现在 Devin 的模型选择器里。")}</small></div>
        <div className={styles.addActions}>
          <Button size="small" disabled={!canAddNativeBinding} onClick={addNativeBinding}>{t("添加模型")}</Button>
        </div>
      </div>
      {!models.length && <small className={styles.reason}>{t("模型库为空。请先在模型库添加一个模型。")}</small>}
      <div className={styles.bindings}>
        {!settings.bindings.length && <small className={styles.empty}>{t("还没有选择模型。点“添加模型”开始。")}</small>}
        {settings.bindings.map((binding, index) => {
          const routes = routesForDisplay(binding);
          const activeRouteId = activeRouteIdForDisplay(binding);
          const hasRoutes = binding.routes.length > 0;
          const name = bindingName(binding);
          const rowKey = binding.model_uid.trim() || `${binding.model_hash}:${index}`;
          const expanded = expandedBindings[rowKey] ?? false;
          return <div className={styles.binding} key={rowKey}>
            <div className={styles.bindingRow}>
              <FormField label={t("模型")} hint={t("来自共享模型库。")}>
                <ModelSelect mode="single" label={t("实际使用")} value={binding.model_hash} options={modelOptions} onChange={(model_hash) => {
                  const model = models.find((item) => item.model_hash === model_hash);
                  updateBinding(index, { model_hash, display_name: model?.display_name ?? binding.display_name, context_window_tokens: model?.context_window_tokens ?? null });
                }} />
              </FormField>
              <div className={styles.bindingRowActions}>
                {!binding.enabled && <span className={styles.bindingPaused}>{t("已停用")}</span>}
                <button type="button" className={styles.more} aria-expanded={expanded} aria-label={t("{model} 的更多设置", { model: name })} onClick={() => setExpandedBindings((current) => ({ ...current, [rowKey]: !expanded }))}>
                  {expanded ? t("收起") : t("高级")}
                </button>
                <button type="button" className={styles.remove} aria-label={t("移除映射：{model}", { model: name })} onClick={() => removeBinding(index)}>{t("移除")}</button>
              </div>
            </div>
            {expanded && <div className={styles.bindingExtra}>
              <div className={styles.routes}>
                <div className={styles.routesHeader}>
                  <span>{t("备用模型")}</span>
                  <Button size="small" disabled={!models.length} onClick={() => appendRoute(index, binding, routes)}>{t("添加备用模型")}</Button>
                </div>
                {!hasRoutes && <small className={styles.routeHint}>{t("未设置备用模型。当前直接使用上面的模型。")}</small>}
                {hasRoutes && !activeRouteId && <small className={styles.routeHint}>{t("尚未选择备用路由；当前仍使用上面的模型。")}</small>}
                {routes.map((route, routeIndex) => <div className={styles.route} key={route.route_id}>
                  <div className={styles.routeFields}>
                    <FormField label={t("路由模型")}><ModelSelect mode="single" label={t("路由模型")} value={route.model_hash} options={modelOptions} onChange={(model_hash) => updateRoute(index, binding, routes, routeIndex, { model_hash })} /></FormField>
                    <FormField label={t("路由名称")}><TextInput value={route.label} onChange={(event) => updateRoute(index, binding, routes, routeIndex, { label: event.target.value })} /></FormField>
                  </div>
                  <div className={styles.routeFooter}>
                    <Switch checked={route.enabled} label={t("启用路由：{route}", { route: routeName(route) })} onChange={(enabled) => updateRoute(index, binding, routes, routeIndex, { enabled })} />
                    <div className={styles.routeActions}>
                      <Button size="small" variant={route.route_id === activeRouteId ? "primary" : "secondary"} aria-label={route.route_id === activeRouteId ? t("{route} 是当前路由", { route: routeName(route) }) : t("把 {route} 设为当前路由", { route: routeName(route) })} disabled={!route.enabled || route.route_id === activeRouteId} onClick={() => commitRoutes(index, routes, route.route_id)}>{route.route_id === activeRouteId ? t("当前路由") : t("设为当前路由")}</Button>
                      <button type="button" className={styles.remove} aria-label={t("移除路由：{route}", { route: routeName(route) })} disabled={!hasRoutes} onClick={() => removeRoute(index, binding, routes, routeIndex)}>{t("移除")}</button>
                    </div>
                  </div>
                </div>)}
              </div>
              <div className={styles.bindingFooter}>
                <Switch checked={binding.enabled} label={t("启用模型映射：{model}", { model: name })} onChange={(enabled) => updateBinding(index, { enabled })} />
                <span />
              </div>
            </div>}
          </div>;
        })}
      </div>
      <div className={styles.settingRow}>
        <div><strong>{t("默认模型")}</strong><small>{t("打开 Devin 时默认选中的模型。")}</small></div>
        <div className={styles.defaultModelControl}>
          <ModelSelect
            mode="single"
            label={t("选择 Devin 默认模型")}
            value={defaultBinding ? defaultBinding.model_uid : ""}
            options={defaultModelOptions}
            disabled={defaultModelOptions.length === 0}
            onChange={(uid) => update("default_model_uid", uid || null)}
          />
        </div>
      </div>
      {defaultMissingReason && <small className={styles.reason}>{defaultMissingReason}</small>}
    </TitledCard>
    {showHostCard && <TitledCard
      title={t("连接到 Devin")}
      description={t("第一次使用时完成一次；之后打开 Devin 就能直接对话。")}
      badge={hostIntegrated
        ? <StatusPill tone="ok">{t("已接入")}</StatusPill>
        : canAutoConnect
          ? <StatusPill tone="idle">{t("尚未接入")}</StatusPill>
          : hostStatus
            ? <StatusPill tone="warn">{t("配置冲突")}</StatusPill>
            : undefined}
    >
      <div className={styles.hostNotes}>
        <p role={hostFailure ? "alert" : undefined}>{hostPrimaryText}</p>
        {extensionUnsupported && hostStatus?.extension.message && <p role="alert">{clientErrorText(hostStatus.extension.message)}</p>}
        {hostStatus && !hostStatus.unconfigured && !hostStatus.configured && hostStatus.message && <p>{clientErrorText(hostStatus.message)}</p>}
      </div>
    </TitledCard>}
    <TitledCard
      title={t("高级")}
      description={t("通常不用改；端口、Devin 文件路径和安装目录都在这里。")}
      collapsible
      storageKey="devin-advanced"
    >
      <div className={styles.fields}>
        {/* 端口是整数：空值或非数字交给服务端拒绝（它会说不清是哪种错），超范围的在这里夹住。 */}
        <FormField label={t("API 端口")}><TextInput type="number" min={1} max={65535} value={settings.api_port} onChange={(event) => update("api_port", portInputValue(event.target.value))} /></FormField>
        <FormField label={t("推理端口")}><TextInput type="number" min={1} max={65535} value={settings.inference_port} onChange={(event) => update("inference_port", portInputValue(event.target.value))} /></FormField>
        <FormField label={t("本地 API 端口")}><TextInput type="number" min={1} max={65535} value={settings.local_api_port} onChange={(event) => update("local_api_port", portInputValue(event.target.value))} /></FormField>
        <FormField label={t("上游 API 地址")} hint={t("登录、账号与遥测等本地不处理的方法会转发到这里；模型请求不经过它。")}>
          <TextInput value={settings.upstream_api_url} onChange={(event) => update("upstream_api_url", event.target.value)} placeholder="https://server.codeium.com" />
        </FormField>
      </div>
      <p className={styles.note}>{t("网关固定监听 127.0.0.1，并限制单次请求体为 24 MiB。Devin 负责执行工具，本机网关负责模型调用和事件转发。")}</p>
      <div className={styles.fields}>
        <FormField label={t("Devin 设置文件路径")} hint={t("留空即自动探测；也可以填 Devin 数据目录，或 User\\settings.json 的完整路径。")}>
          <TextInput value={hostPath} onChange={(event) => rememberHostPath(event.target.value)} onBlur={(event) => flushHostPath(event.target.value)} placeholder={t("留空自动探测")} />
        </FormField>
        <FormField label={t("Devin 安装目录")} hint={t("留空即自动探测；也可以填安装目录，或其中 extension.js 的完整路径。")}>
          <TextInput value={hostInstallRoot} onChange={(event) => rememberHostInstallRoot(event.target.value)} onBlur={(event) => flushHostInstallRoot(event.target.value)} placeholder={t("留空自动探测")} />
        </FormField>
      </div>
      <details className={styles.technical}>
        <summary>{t("技术详情")}</summary>
        <div className={styles.technicalBody}>
          {hostStatus?.path && <p>{t("设置文件：{path}", { path: hostStatus.path })}</p>}
          {hostStatus?.extension.path && <p>{t("扩展文件：{path}", { path: hostStatus.extension.path })}</p>}
          {hostStatus?.configured && hostStatus.ports && <p>{t("端口：{ports}", { ports: `${hostStatus.ports.api_port} / ${hostStatus.ports.inference_port}` })}</p>}
          {hostStatus && <p>{t("兼容补丁：{state}", { state: hostStatus.extension.patched ? t("已写入") : t("未写入") })}</p>}
          {hostStatus?.extension.version && <p>{t("Devin 版本：{version}", { version: hostStatus.extension.version })}</p>}
          {hostStatus?.message && <p>{clientErrorText(hostStatus.message)}</p>}
          {hostStatus?.extension.message && <p>{clientErrorText(hostStatus.extension.message)}</p>}
        </div>
      </details>
      {!hostRestorable && <p className={styles.note}>{t("只有本应用写入的配置才能一键恢复。")}</p>}
    </TitledCard>
    <HarnessCapabilities client="devin" collapsible />
    {view.pendingChanges && <div className={styles.saveBar}>
      <span>{t("有未保存的改动")}</span>
      <div className={styles.addActions}>
        <Button size="small" disabled={saving} onClick={() => setSettings(saved)}>{t("放弃改动")}</Button>
        <Button size="small" variant="primary" disabled={saving || Boolean(defaultMissingReason)} onClick={() => void save()}>{saving ? t("保存中…") : t("保存")}</Button>
      </div>
    </div>}
  </div>;
  return <>    <ConfirmDialog
      open={confirmForceHost}
      destructive
      title={t("强制接管 Devin？")}
      cancelLabel={t("取消")}
      confirmLabel={t("覆盖并接管")}
      busy={hostBusy}
      onCancel={() => setConfirmForceHost(false)}
      onConfirm={() => { setConfirmForceHost(false); void applyHostConfiguration(true); }}
    >
      <p>{t("Devin 的设置里已经有一组外部端点。强制接管会覆盖它们并保留备份；撤销接管时恢复。请先完全退出 Devin。")}</p>
    </ConfirmDialog>
    <PageContent
    title={<PageTitle
      title="Devin"
      status={loadError
        ? <StatusPill tone="bad">{t("读取失败")}</StatusPill>
        : <StatusPill tone={view.summary.tone}>{view.summary.label}</StatusPill>}
      meta={t("选择模型并接入 Devin，之后直接在 Devin 里对话")}
    />}
    sections={[{ key: "devin", estimatedHeight: 1120, content }]}
    />
  </>;
}
