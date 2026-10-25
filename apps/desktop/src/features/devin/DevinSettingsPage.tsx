import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, type DevinBindingKind, type DevinHostStatus, type DevinModelBinding, type DevinModelChoice, type DevinRoute, type DevinSettings, type Model } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { FormField, SecretTextInput, TextInput } from "../../shared/ui/FormControls";
import { ModelSelect } from "../../shared/ui/ModelSelect";
import { Combobox, Select } from "../../shared/ui/Select";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import { StatusHero } from "../../shared/ui/StatusHero";
import { PageContent } from "../../shell/layout/PageContent";
import { PageTitle } from "../../shared/ui/PageTitle";
import { StatusPill } from "../../shared/ui/StatusPill";
import { clientErrorText, writeFailureText } from "../harness/harnessErrorText";
import { devinConnectionView } from "./devinConnectionView";
import { applyDevinHostConfiguration, canRestoreDevinHost } from "./devinHostApply";
import { appendRouteForBinding, keepActiveRouteId, nextModelUid } from "./devinBindingEdits";
import { devinDefaultModelOptions, devinModelOptions } from "./devinModelOptions";
import { appStore, useAppStoreSelector } from "../../shared/store/appStore";
import { getUiPreferences, saveUiPreferences, useUiPreferences } from "../../shared/store/uiPreferences";
import { probeHostPath } from "./hostPathProbe";
import { HarnessCapabilities } from "../harness/HarnessCapabilities";
import styles from "./DevinSettingsPage.module.scss";

const emptySettings: DevinSettings = {
  enabled: false,
  auth_token: "",
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
  return String(binding.model_uid ?? "").trim() || binding.display_name || binding.model_hash || "";
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
  const [models, setModels] = useState<Model[]>([]);
  // Devin 选择器里的模型（名字 + 标识），映射的 UID 必须从这里来。
  const [modelChoices, setModelChoices] = useState<DevinModelChoice[]>([]);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [hostPath, setHostPath] = useState(readHostPath);
  const [hostInstallRoot, setHostInstallRoot] = useState(readHostInstallRoot);
  const [hostStatus, setHostStatus] = useState<DevinHostStatus | null>(null);
  const [hostFailure, setHostFailure] = useState<string | null>(null);
  const [hostBusy, setHostBusy] = useState(false);
  const [hostNeedsInspect, setHostNeedsInspect] = useState(false);
  // Which mapping rows are showing their optional fields. Collapsed by default: the
  // page should read as two decisions per mapping, not as a form of five fields.
  const [expandedBindings, setExpandedBindings] = useState<Record<string, boolean>>({});
  const devinStatus = useAppStoreSelector((state) => state.devinStatus);

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
    void Promise.all([api.devinSettings(), api.models()]).then(([nextSettings, nextModels]) => {
      setSettings(nextSettings);
      setSaved(nextSettings);
      setModels(nextModels);
    }).catch((cause) => message.error(clientErrorText(cause))).finally(() => setLoading(false));
  }, [message]);

  // 模型 UID 只能从 Devin 自己认识的标识里选：手打的标识永远不会被请求。列表读不到
  // 只影响"能不能选"，映射本身仍然可以手填，所以这里失败不弹错。
  useEffect(() => {
    if (loading) return;
    const extensionPath = hostStatus?.extension.path;
    // 状态读不到时仍按记住的安装目录（或自动探测）取一次：路径失效只影响接入，
    // 不该把已有映射的 UID 选择器也清空。
    const source = extensionPath || hostInstallRoot.trim();
    let cancelled = false;
    void api.devinModelUids(source || undefined)
      .then((table) => {
        if (!cancelled) setModelChoices(table.choices);
      })
      .catch(() => {
        // 读不到只说明这次的位置无效，不应把已经知道的 Devin 模型名降级成原始 UID。
        // 下一次成功读取会整体替换这份列表。
      });
    return () => {
      cancelled = true;
    };
  }, [loading, hostStatus?.extension.path]);

  useEffect(() => {
    if (loading) return;
    void appStore.refreshDevinStatus();
  }, [loading]);

  // 设置文件路径与安装目录不再要求用户填写：服务端自己找到它们，再回填到界面。
  //
  // 这里刻意只跑一次，用的是"打开页面时记住的那两个输入"：把它们列进依赖会让用户
  // 每敲一个字符就打一次探测请求。初始值由 useState 给出，所以闭包里拿到的是该用的值。
  //
  // 记住的路径失效（卸载、换盘、搬到别处）不是用户的操作错误，因此不弹红条：两个输入
  // 各自失效时互不牵连，都失败才退回自动探测；仍失败才把原因写在卡片里。
  useEffect(() => {
    if (loading) return;
    let cancelled = false;
    void probeHostPath(hostPath, api.devinHostStatus, hostInstallRoot)
      .then((probe) => {
        if (cancelled) return;
        setHostStatus(probe.status);
        setHostFailure(probe.failure);
        setHostNeedsInspect(false);
        if (probe.status && probe.status.path !== hostPath.trim()) {
          setHostPath(probe.status.path);
          writeHostPath(probe.status.path);
        }
        if (probe.status && hostInstallRoot.trim() && !probe.matched.installRoot) {
          // 记住的安装目录已经失效，自动探测成功：清掉旧值，别让输入框和实际状态互相矛盾。
          setHostInstallRoot("");
          writeHostInstallRoot("");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [loading]);

  const rememberHostPath = (value: string) => {
    setHostPath(value);
    setHostStatus(null);
    setHostFailure(null);
    setHostNeedsInspect(true);
    writeHostPath(value);
  };
  const rememberHostInstallRoot = (value: string) => {
    setHostInstallRoot(value);
    setHostStatus(null);
    setHostFailure(null);
    setHostNeedsInspect(true);
    writeHostInstallRoot(value);
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
        const nextKind = patch.kind ?? binding.kind;
        defaultModelUid = nextEnabled && nextKind === "standard" && nextUid ? nextUid : null;
      }
      return {
        ...current,
        default_model_uid: defaultModelUid,
        bindings: current.bindings.map((item, itemIndex) => itemIndex === index ? { ...item, ...patch } : item),
      };
    });
  };
  const addBinding = () => {
    setSettings((current) => {
      const modelUid = nextModelUid(current.bindings, modelChoices);
      if (!modelUid) return current;
      const model = models[0];
      return {
        ...current,
        default_model_uid: current.bindings.length === 0 ? modelUid : current.default_model_uid,
        bindings: [...current.bindings, {
          model_uid: modelUid,
          model_hash: model?.model_hash ?? "",
          display_name: model?.display_name ?? "",
          context_window_tokens: model?.context_window_tokens ?? null,
          enabled: true,
          kind: "standard",
          routes: [],
          active_route_id: null,
        }],
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
        writeHostPath(status.path);
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
  const applyHostConfiguration = async () => {
    setHostBusy(true);
    try {
      const outcome = await applyDevinHostConfiguration(hostPath, hostInstallRoot);
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
      setHostFailure(null);
      if (outcome.status.path !== hostPath.trim()) {
        setHostPath(outcome.status.path);
        writeHostPath(outcome.status.path);
      }
      // 成功消息只在状态读回来之后才发：设置落盘和"界面知道设置落盘了"是两件事。
      message(t("Devin 接入配置已写入；重新打开 Devin 后生效"), { duration: 5_000 });
    } finally {
      setHostBusy(false);
      setHostNeedsInspect(false);
    }
  };
  const restoreHostConfiguration = async () => {
    if (!hostStatus || !canRestoreDevinHost(hostStatus)) return;
    try {
      setHostBusy(true);
      await api.restoreDevinHostConfiguration(hostStatus.path, hostInstallRoot);
      const status = await api.devinHostStatus(hostStatus.path, hostInstallRoot);
      setHostStatus(status);
      setHostFailure(null);
      if (status.path !== hostPath.trim()) {
        setHostPath(status.path);
        writeHostPath(status.path);
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
      const next = await api.setDevinSettings(settings);
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
  const saveAndConnect = async () => {
    const next = await persistSettings(false);
    if (!next) return;
    if (next.enabled && hostStatus?.unconfigured && hostStatus.extension.clean) {
      await applyHostConfiguration();
      return;
    }
    message(t("设置已保存，Devin 会使用新模型。"), { duration: 4_000 });
  };
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
  const defaultModelOptions = devinDefaultModelOptions(settings.bindings, models, modelChoices);
  const defaultBinding = settings.bindings.find((binding) => (
    binding.enabled
    && binding.kind === "standard"
    && binding.model_uid.trim() === settings.default_model_uid?.trim()
  )) ?? null;
  const defaultMissingReason = settings.enabled && !defaultBinding
    ? settings.default_model_uid
      ? t("当前默认模型已停用或不可用，请重新选择。")
      : t("请选择一个默认模型，保存后才能使用。")
    : null;
  const nextBindingUid = nextModelUid(settings.bindings, modelChoices);
  const canAddBinding = models.length > 0 && nextBindingUid !== null;
  // 选择器显示 Devin 里的模型名，写回的仍是客户端随后会请求的标识。
  const uidOptions = modelChoices.map((choice) => ({ value: choice.uid, label: choice.name }));
  const kindOptions = [
    { value: "standard" as DevinBindingKind, label: t("标准") },
    { value: "context_compression" as DevinBindingKind, label: t("上下文压缩") },
  ];
  const hostIntegrated = Boolean(
    hostStatus?.configured
    && hostStatus.extension.patched
    && hostStatus.ports?.api_port === saved.api_port
    && hostStatus.ports?.inference_port === saved.inference_port,
  );
  const hostRestorable = canRestoreDevinHost(hostStatus);
  const extensionMissing = hostStatus != null && hostStatus.extension.path == null;
  const extensionUnsupported = hostStatus != null && hostStatus.extension.path != null && !hostStatus.extension.compatible;
  const canAutoConnect = Boolean(hostStatus?.unconfigured && hostStatus.extension.clean);
  const connectDisabled = saving
    || hostBusy
    || (settings.enabled && !settings.bindings.length)
    || (settings.enabled && !defaultBinding)
    || (!view.pendingChanges && !canAutoConnect);
  const connectLabel = hostBusy
    ? t("处理中…")
    : canAutoConnect
      ? t("保存并接入")
      : hostIntegrated && !view.pendingChanges
        ? t("已接入")
        : hostIntegrated
          ? t("保存设置")
        : t("保存");
  const hostPrimaryText = hostFailure
    ? clientErrorText(hostFailure)
    : hostNeedsInspect
      ? t("路径已修改。点“重新检查”确认新的位置。")
      : !hostStatus
        ? t("正在检查 Devin 配置…")
        : hostIntegrated
          ? t("已连接。重新打开 Devin 就可以对话。")
          : hostStatus.unconfigured && hostStatus.extension.clean
            ? t("还差一步：关闭 Devin，然后点右上角“保存并接入”。")
            : extensionMissing
              ? t("找不到 Devin。请先安装或打开一次 Devin；如果装在自定义位置，请在“高级”里填写安装目录。")
              : extensionUnsupported
                ? t("当前 Devin 版本暂不支持自动接入。")
                : hostStatus.configured
                  ? t("Devin 目前指向其他地址。请在“高级”里恢复原设置后再试。")
                  : t("暂时不能自动接入，请查看下面的技术详情。");
  const content = loading ? <div className={styles.loading}>{t("加载中…")}</div> : <div className={styles.page}>
    <TitledCard title={t("接入状态")} action={<Button size="small" onClick={() => navigate("/calls")}>{t("查看调用记录")}</Button>}>
      <StatusHero
        connected={view.hero.connected}
        title={view.hero.title}
        description={view.hero.description}
        stages={view.hero.stages}
        steps={view.hero.steps}
        footnotes={view.hero.footnotes}
      />
    </TitledCard>
    <TitledCard
      title={t("使用哪个模型")}
      description={t("Devin 的对话会由你选择的模型回答。")}
      collapsible={false}
    >
      <div className={styles.settingRow}>
        <div><strong>{t("使用自选模型")}</strong><small>{t("开启后，Devin 的请求会交给你下面选择的模型。")}</small></div>
        <Switch checked={settings.enabled} label={t("使用自选模型")} onChange={(enabled) => update("enabled", enabled)} />
      </div>
      <div className={styles.settingRow}>
        <div><strong>{t("对话模型")}</strong><small>{t("左侧是 Devin 里的模型，右侧是实际使用的模型。")}</small></div>
        <Button size="small" disabled={!canAddBinding} onClick={addBinding}>{t("添加模型")}</Button>
      </div>
      {!models.length && <small className={styles.reason}>{t("模型库为空。请先在模型库添加一个模型。")}</small>}
      {models.length > 0 && modelChoices.length > 0 && nextBindingUid === null && <small className={styles.reason}>{t("所有 Devin 模型都已添加。")}</small>}
      <div className={styles.bindings}>
        {!settings.bindings.length && <small className={styles.empty}>{t("还没有选择模型。点“添加模型”开始。")}</small>}
        {settings.bindings.map((binding, index) => {
          const routes = routesForDisplay(binding);
          const activeRouteId = activeRouteIdForDisplay(binding);
          const hasRoutes = binding.routes.length > 0;
          const isCompression = binding.kind === "context_compression";
          const name = bindingName(binding);
          // 行的 key 不能随输入变化：原来用的是 `model_uid-index`，于是每敲一个字符 key
          // 就变一次，React 把整行卸载重建，输入框随即失焦——UID 只能"敲一个字点一次"。
          // 下标在编辑期间是稳定的，只有增删行时才会移动。
          const rowKey = String(index);
          const expanded = expandedBindings[rowKey] ?? false;
          // A mapping is a two-value decision: which UID Devin asks for, and which
          // model answers. Display name, context size, kind and fallback routes are
          // kept behind the disclosure, because a page that shows every field makes
          // the two that matter impossible to find.
          return <div className={styles.binding} key={rowKey}>
            <div className={styles.bindingRow}>
              <FormField label={t("Devin 里的模型")} hint={t("一般保持默认即可。")}><Combobox value={binding.model_uid} options={uidOptions} onChange={(model_uid) => updateBinding(index, { model_uid })} /></FormField>
              <span className={styles.mappingArrow} aria-hidden="true">→</span>
              <ModelSelect mode="single" label={t("实际使用")} value={binding.model_hash} options={modelOptions} onChange={(model_hash) => {
                const model = models.find((item) => item.model_hash === model_hash);
                updateBinding(index, { model_hash, display_name: model?.display_name ?? binding.display_name, context_window_tokens: model?.context_window_tokens ?? null });
              }} />
              <div className={styles.bindingRowActions}>
                {!binding.enabled && <span className={styles.bindingPaused}>{t("已停用")}</span>}
                <button type="button" className={styles.more} aria-expanded={expanded} aria-label={t("{model} 的更多设置", { model: name })} onClick={() => setExpandedBindings((current) => ({ ...current, [rowKey]: !expanded }))}>
                  {expanded ? t("收起") : t("高级")}
                </button>
                <button type="button" className={styles.remove} aria-label={t("移除映射：{model}", { model: name })} onClick={() => removeBinding(index)}>{t("移除")}</button>
              </div>
            </div>
            {expanded && <div className={styles.bindingExtra}>
              <div className={styles.bindingFields}>
                <FormField label={t("上下文 token")} hint={t("一般不用改；留空表示使用模型库中的设置。")}><TextInput type="number" min={1} value={binding.context_window_tokens ?? ""} onChange={(event) => updateBinding(index, { context_window_tokens: event.target.value ? Number(event.target.value) : null })} /></FormField>
                <FormField label={t("绑定类型")} hint={t("上下文压缩绑定必须是单线路；切换为该类型会清空候选路由。")}>
                  <Select value={binding.kind} options={kindOptions} ariaLabel={t("绑定类型")} onChange={(kind) => {
                    const nextKind = kind as DevinBindingKind;
                    updateBinding(index, nextKind === "context_compression" ? { kind: nextKind, routes: [], active_route_id: null } : { kind: nextKind });
                  }} />
                </FormField>
              </div>
              {isCompression ? <small className={styles.routeHint}>{t("上下文压缩绑定只使用上面的模型，不参与候选路由。")}</small> : <div className={styles.routes}>
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
              </div>}
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
    <HarnessCapabilities client="devin" />
    <TitledCard
      title={t("连接到 Devin")}
      description={t("第一次使用时完成一次；之后打开 Devin 就能直接对话。")}
      badge={hostIntegrated
        ? <StatusPill tone="ok">{t("已接入")}</StatusPill>
        : hostStatus?.unconfigured && hostStatus.extension.clean
          ? <StatusPill tone="idle">{t("尚未接入")}</StatusPill>
          : hostStatus
            ? <StatusPill tone="warn">{t("配置冲突")}</StatusPill>
            : undefined}
      action={<div className={styles.hostActions}>
        <Button size="small" disabled={hostBusy} onClick={() => void inspectHost()}>{hostBusy ? t("检查中…") : t("重新检查")}</Button>
        <Button size="small" variant="primary" disabled={connectDisabled} onClick={() => void saveAndConnect()}>{connectLabel}</Button>
      </div>}
    >
      <div className={styles.hostNotes}>
        <p role={hostFailure ? "alert" : undefined}>{hostPrimaryText}</p>
        {extensionUnsupported && hostStatus?.extension.message && <p role="alert">{clientErrorText(hostStatus.extension.message)}</p>}
        {hostStatus && !hostStatus.unconfigured && !hostStatus.configured && hostStatus.message && <p>{clientErrorText(hostStatus.message)}</p>}
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
      </div>
      <p className={styles.note}>{t("登录、套餐和额度仍由 Devin 管理。")}</p>
    </TitledCard>
    <TitledCard
      title={t("高级")}
      description={t("通常不用改；端口、凭据、Devin 文件路径和安装目录都在这里。")}
      action={<Button size="small" disabled={!hostRestorable || hostBusy} onClick={() => void restoreHostConfiguration()}>{t("恢复原设置")}</Button>}
      collapsible
      storageKey="devin-advanced"
    >
      <div className={styles.fields}>
        <FormField label={t("控制令牌")} hint={t("可选；填了之后，Devin 的请求要带同一个令牌（请求头 x-devin-router-token，或 Authorization: Bearer）。")}>
          <SecretTextInput value={settings.auth_token} onChange={(event) => update("auth_token", event.target.value)} placeholder={t("留空表示仅依赖本机回环访问")} />
        </FormField>
        {/* 端口是整数：空值或非数字交给服务端拒绝（它会说不清是哪种错），超范围的在这里夹住。 */}
        <FormField label={t("API 端口")}><TextInput type="number" min={1} max={65535} value={settings.api_port} onChange={(event) => update("api_port", portValue(event.target.value))} /></FormField>
        <FormField label={t("推理端口")}><TextInput type="number" min={1} max={65535} value={settings.inference_port} onChange={(event) => update("inference_port", portValue(event.target.value))} /></FormField>
        <FormField label={t("本地 API 端口")}><TextInput type="number" min={1} max={65535} value={settings.local_api_port} onChange={(event) => update("local_api_port", portValue(event.target.value))} /></FormField>
        <FormField label={t("上游 API 地址")} hint={t("登录、账号与遥测等本地不处理的方法会转发到这里；模型请求不经过它。")}>
          <TextInput value={settings.upstream_api_url} onChange={(event) => update("upstream_api_url", event.target.value)} placeholder="https://server.codeium.com" />
        </FormField>
      </div>
      <p className={styles.note}>{t("网关固定监听 127.0.0.1，并限制单次请求体为 24 MiB。Devin 负责执行工具，本机网关负责模型调用和事件转发。")}</p>
      <div className={styles.fields}>
        <FormField label={t("Devin 设置文件路径")} hint={t("留空即自动探测；也可以填 Devin 数据目录，或 User\\settings.json 的完整路径。")}>
          <TextInput value={hostPath} onChange={(event) => rememberHostPath(event.target.value)} placeholder={t("留空自动探测")} />
        </FormField>
        <FormField label={t("Devin 安装目录")} hint={t("留空即自动探测；也可以填安装目录，或其中 extension.js 的完整路径。")}>
          <TextInput value={hostInstallRoot} onChange={(event) => rememberHostInstallRoot(event.target.value)} placeholder={t("留空自动探测")} />
        </FormField>
      </div>
      {!hostRestorable && <p className={styles.note}>{t("只有本应用写入的配置才能一键恢复。")}</p>}
    </TitledCard>
    {view.pendingChanges && <div className={styles.saveBar}>
      <span>{t("有未保存的改动")}</span>
      <Button variant="primary" size="small" disabled={saving || Boolean(defaultMissingReason)} onClick={save}>{saving ? t("保存中…") : t("保存更改")}</Button>
    </div>}
  </div>;
  return <PageContent
    title={<PageTitle
      title="Devin"
      status={<StatusPill tone={view.summary.tone}>{view.summary.label}</StatusPill>}
      meta={t("选择模型并接入 Devin，之后直接在 Devin 里对话")}
    />}
    sections={[{ key: "devin", estimatedHeight: 1120, content }]}
  />;
}

/**
 * 端口输入框的取值：空输入按 0 交给服务端拒绝（它会说"端口不能为 0"），超范围的夹到 1..65535。
 *
 * 原来直接用 `Number(...)`：输入框清空时 `Number("")` 是 0、乱输是 `NaN`（JSON 里会变成
 * `null`），服务端只能回一句英文的 422，界面看起来就是"保存没反应"。
 */
function portValue(raw: string): number {
  const value = Number(raw);
  if (!Number.isFinite(value)) return 0;
  return Math.min(65_535, Math.max(0, Math.trunc(value)));
}
