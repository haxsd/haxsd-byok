import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, type DevinBindingKind, type DevinHostPatchStatus, type DevinModelBinding, type DevinModelChoice, type DevinRoute, type DevinSettings, type Model } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { FormField, SecretTextInput, TextInput } from "../../shared/ui/FormControls";
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
import { applyDevinHostPatch } from "./devinPatchApply";
import { getUiPreferences, saveUiPreferences, useUiPreferences } from "../../shared/store/uiPreferences";
import { probeHostPath } from "./hostPathProbe";
import styles from "./DevinSettingsPage.module.scss";

const emptySettings: DevinSettings = {
  enabled: false,
  auth_token: "",
  api_port: 43_110,
  inference_port: 43_111,
  local_api_port: 43_112,
  upstream_api_url: "https://server.self-serve.windsurf.com",
  bindings: [],
};

/** 旧设置只有主模型哈希，界面把它显示成一条合成的主路由，直到用户真正编辑绑定。 */
const legacyRouteId = "primary";

/** 宿主文件路径必须记住：状态面板、检查、打补丁全靠它，刷新即丢会让人以为功能坏了。 */
/** 存在服务端（原来在 localStorage：端口一变路径就没了），见 shared/store/uiPreferences.ts。 */
function readHostPath(): string {
  return getUiPreferences().devinHostPath ?? "";
}

/** 记住宿主路径；写失败不影响当前会话。 */
function writeHostPath(path: string) {
  if (path.trim()) void saveUiPreferences({ devinHostPath: path });
}

function routesForDisplay(binding: DevinModelBinding): DevinRoute[] {
  if (binding.routes.length) return binding.routes;
  return [{ route_id: legacyRouteId, model_hash: binding.model_hash, label: binding.display_name, enabled: true }];
}

function activeRouteIdForDisplay(binding: DevinModelBinding): string | null {
  return binding.routes.length ? binding.active_route_id : legacyRouteId;
}

function pickActiveRouteId(routes: DevinRoute[], preferred: string | null): string | null {
  if (preferred && routes.some((route) => route.route_id === preferred && route.enabled)) return preferred;
  return routes.find((route) => route.enabled)?.route_id ?? null;
}

function nextRouteId(routes: DevinRoute[]): string {
  const used = new Set(routes.map((route) => route.route_id));
  let index = routes.length + 1;
  while (used.has(`route-${index}`)) index += 1;
  return `route-${index}`;
}

/**
 * 新映射的默认 UID：优先挑真实列表里还没被占用的那一个。
 *
 * 默认值必须取自 Devin 自己认识的标识，自己编一个名字（曾经是 `cursor-byok-N`）
 * 的结果是模型选择器里根本没有它，用户怎么配都不会生效；而重复的 UID 又会被服务端
 * 拒绝，所以要在未占用的里面挑。
 */
function nextModelUid(bindings: DevinModelBinding[], choices: DevinModelChoice[]): string {
  const fallback = "swe-1-6-slow";
  if (!choices.length) return fallback;
  const used = new Set(bindings.map((binding) => String(binding.model_uid ?? "").trim()));
  const free = choices.find((choice) => !used.has(choice.uid));
  return (free ?? choices[0]).uid;
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
  const [hostStatus, setHostStatus] = useState<DevinHostPatchStatus | null>(null);
  const [hostFailure, setHostFailure] = useState<string | null>(null);
  const [hostBusy, setHostBusy] = useState(false);
  // Which mapping rows are showing their optional fields. Collapsed by default: the
  // page should read as two decisions per mapping, not as a form of five fields.
  const [expandedBindings, setExpandedBindings] = useState<Record<string, boolean>>({});
  const [gatewayPortsUp, setGatewayPortsUp] = useState<boolean | null>(null);
  const [devinCalls, setDevinCalls] = useState<number | null>(null);

  // 偏好是启动时异步读回来的：值到了之后补上宿主路径（用户没在这期间自己填过才算）。
  const rememberedHostPath = useUiPreferences().devinHostPath ?? "";
  useEffect(() => {
    if (rememberedHostPath) setHostPath((current) => current || rememberedHostPath);
  }, [rememberedHostPath]);

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
    let cancelled = false;
    void api.devinModelUids(hostPath.trim() || undefined)
      .then((table) => {
        if (!cancelled) setModelChoices(table.choices);
      })
      .catch(() => {
        if (!cancelled) setModelChoices([]);
      });
    return () => {
      cancelled = true;
    };
  }, [loading]);

  // 状态面板要回答"现在通没通"。端口是否在听只能由服务端回答：网关端口属于另一个源，
  // 浏览器直接探测会被 CORS 拒绝，从而把健康的网关误报成"端口未监听"。监听状态跟着
  // 已保存的开关走：草稿上的开关还没落库，不代表网关会跟着变。
  useEffect(() => {
    if (loading) return;
    let cancelled = false;
    void api.devinStatus()
      .then((status) => {
        if (cancelled) return;
        setGatewayPortsUp(status.listening);
        setDevinCalls(status.calls);
      })
      .catch(() => {
        if (cancelled) return;
        setGatewayPortsUp(null);
        setDevinCalls(null);
      });
    return () => {
      cancelled = true;
    };
  }, [loading, saved.enabled]);

  // 宿主路径不再要求用户填写：服务端自己找到安装位置，再回填到界面。
  //
  // 这里刻意只跑一次，用的是"打开页面时记住的那条路径"：把 hostPath 列进依赖会让
  // 用户每敲一个字符就打一次探测请求。路径本身由 useState 的初始值给出，所以闭包里
  // 拿到的就是该用的值，而不是某个中间状态。
  //
  // 记住的路径失效（卸载、换盘、搬到别处）不是用户的操作错误，因此不弹红条：先按它
  // 试一次，失败就退回自动探测；两次都不成才把原因写在卡片里。
  useEffect(() => {
    if (loading) return;
    let cancelled = false;
    void probeHostPath(hostPath, api.devinHostStatus)
      .then((probe) => {
        if (cancelled) return;
        setHostStatus(probe.status);
        setHostFailure(probe.failure);
        if (probe.status && probe.status.path !== hostPath.trim()) {
          setHostPath(probe.status.path);
          writeHostPath(probe.status.path);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [loading]);

  const rememberHostPath = (value: string) => {
    setHostPath(value);
    setHostStatus(null);
    writeHostPath(value);
  };

  const update = <K extends keyof DevinSettings>(key: K, value: DevinSettings[K]) => {
    setSettings((current) => ({ ...current, [key]: value }));
  };
  const updateBinding = (index: number, patch: Partial<DevinModelBinding>) => {
    setSettings((current) => ({
      ...current,
      bindings: current.bindings.map((binding, itemIndex) => itemIndex === index ? { ...binding, ...patch } : binding),
    }));
  };
  const addBinding = () => {
    const model = models[0];
    update("bindings", [...settings.bindings, {
      model_uid: nextModelUid(settings.bindings, modelChoices),
      model_hash: model?.model_hash ?? "",
      display_name: model?.display_name ?? "",
      context_window_tokens: model?.context_window_tokens ?? null,
      enabled: true,
      kind: "standard",
      routes: [],
      active_route_id: null,
    }]);
  };
  const removeBinding = (index: number) => update("bindings", settings.bindings.filter((_, itemIndex) => itemIndex !== index));
  const commitRoutes = (index: number, routes: DevinRoute[], activeRouteId: string | null) => {
    updateBinding(index, routes.length ? { routes, active_route_id: pickActiveRouteId(routes, activeRouteId) } : { routes: [], active_route_id: null });
  };
  const updateRoute = (index: number, binding: DevinModelBinding, routes: DevinRoute[], routeIndex: number, patch: Partial<DevinRoute>) => {
    commitRoutes(index, routes.map((route, itemIndex) => itemIndex === routeIndex ? { ...route, ...patch } : route), activeRouteIdForDisplay(binding));
  };
  const appendRoute = (index: number, binding: DevinModelBinding, routes: DevinRoute[]) => {
    const model = models[0];
    commitRoutes(index, [...routes, {
      route_id: nextRouteId(routes),
      model_hash: model?.model_hash ?? binding.model_hash,
      label: model?.display_name ?? "",
      enabled: true,
    }], activeRouteIdForDisplay(binding));
  };
  const removeRoute = (index: number, binding: DevinModelBinding, routes: DevinRoute[], routeIndex: number) => {
    commitRoutes(index, routes.filter((_, itemIndex) => itemIndex !== routeIndex), activeRouteIdForDisplay(binding));
  };
  const inspectHost = async () => {
    try {
      setHostBusy(true);
      setHostStatus(await api.devinHostStatus(hostPath));
      setHostFailure(null);
    } catch (cause) {
      // 用户主动点"检查宿主"，路径无效就要立刻说清楚（说法由界面给，不显示服务端英文原句）。
      message.error(clientErrorText(cause));
      setHostFailure(clientErrorText(cause));
      setHostStatus(null);
    } finally {
      setHostBusy(false);
    }
  };
  const applyHostPatch = async () => {
    setHostBusy(true);
    try {
      const outcome = await applyDevinHostPatch(hostPath);
      if (outcome.kind === "write-failed") {
        // 补丁没写下去：文件保持原样，"应用补丁"仍然可用。
        const text = writeFailureText(outcome.cause);
        message.error(text);
        setHostFailure(text);
        return;
      }
      if (outcome.kind === "status-unreadable") {
        // 补丁已经写进文件，是读状态失败。这里不能说"应用失败"——那会诱导用户再点一次，
        // 而第二次会被服务端以"已经是打过补丁的版本"拒绝。清掉旧状态并指向「检查宿主」。
        setHostStatus(null);
        const text = t("补丁已写入宿主文件，但状态没能读回来：点「检查宿主」确认结果，不要重复应用。");
        setHostFailure(text);
        message.error(text, { duration: 8_000 });
        return;
      }
      setHostStatus(outcome.status);
      setHostFailure(null);
      // 成功消息只在状态读回来之后才发：补丁落盘和"界面知道补丁落盘了"是两件事。
      message(t("Devin 宿主补丁已应用；重启 Devin 后生效"), { duration: 5_000 });
    } finally {
      setHostBusy(false);
    }
  };
  const restoreHostPatch = async () => {
    if (!hostStatus?.restore_receipt) return;
    try {
      setHostBusy(true);
      await api.restoreDevinHostPatch(hostStatus.path);
      setHostStatus(await api.devinHostStatus(hostPath));
      setHostFailure(null);
      message(t("Devin 宿主文件已恢复"));
    } catch (cause) {
      message.error(writeFailureText(cause));
    } finally {
      setHostBusy(false);
    }
  };
  const save = async () => {
    try {
      setSaving(true);
      const next = await api.setDevinSettings(settings);
      setSettings(next);
      setSaved(next);
      message(t("Devin 设置已保存，重启软件后监听端口生效"), { duration: 5_000 });
    } catch (cause) {
      message.error(clientErrorText(cause));
    } finally {
      setSaving(false);
    }
  };
  // 接入状态只看已保存的配置与服务端报告的监听状态：草稿上的改动只让底部出现
  // "有未保存的改动"，不能让状态面板声称某个开关已经生效。
  const view = devinConnectionView({
    draft: settings,
    saved,
    models,
    hostStatus,
    gatewayListening: gatewayPortsUp,
    devinCalls,
  });

  const modelOptions = models.map((model) => ({ value: model.model_hash, label: `${model.display_name} · ${model.model_hash.slice(0, 8)}` }));
  // 选择器显示 Devin 里的模型名，写回的仍是客户端随后会请求的标识。
  const uidOptions = modelChoices.map((choice) => ({ value: choice.uid, label: choice.name }));
  const kindOptions = [
    { value: "standard" as DevinBindingKind, label: t("标准") },
    { value: "context_compression" as DevinBindingKind, label: t("上下文压缩") },
  ];
  // 「应用补丁」被禁用的原因：没有一行说明，用户只看到一个点不动的按钮。判断顺序
  // 与按钮的 disabled 表达式一致；读不到宿主文件时上面那句说明已经解释了按钮为什么
  // 不可用，这里就不再补一句。
  const patchDisabled = !hostStatus?.clean || !saved.enabled || view.pendingChanges || hostBusy;
  const patchReason = !patchDisabled
    ? null
    : hostBusy
      ? t("正在检查或写入，稍候。")
      : !saved.enabled
        ? t("先启用并保存 Devin 网关，再应用补丁。")
        : view.pendingChanges
          ? t("有未保存的改动：先保存设置，再应用补丁。")
          : hostStatus
            ? t("当前宿主文件不是可打补丁的原版；按上面的说明恢复或改用其他路径。")
            : null;
  // 「恢复原文件」只在补丁由本应用落下、且记录未被改动时可用。
  const restoreReason = hostStatus?.patched && !hostStatus.restore_receipt
    ? t("恢复记录不可用：补丁不是本应用打的，或记录已被改动；只能手工用旁边的备份文件恢复。")
    : null;
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
      title={t("基础设置")}
      description={t("网关开关与模型映射")}
      collapsible={false}
    >
      <div className={styles.settingRow}>
        <div><strong>{t("启用 Devin 网关")}</strong><small>{t("关闭时不会打开任何 Devin 端口，也不会影响其他接入模块。")}</small></div>
        <Switch checked={settings.enabled} label={t("启用 Devin 网关")} onChange={(enabled) => update("enabled", enabled)} />
      </div>
      <div className={styles.settingRow}>
        <div><strong>{t("模型映射")}</strong><small>{t("把 Devin 的模型标识指向模型库里的模型；Devin 只会用到这里配置的映射。")}</small></div>
        <Button size="small" disabled={!models.length} onClick={addBinding}>{t("添加映射")}</Button>
      </div>
      {/* 模型库为空时「添加映射」是灰的：把原因写在按钮下面，而不是让用户猜。 */}
      {!models.length && <small className={styles.reason}>{t("模型库还是空的：先在模型库添加模型，再回来添加映射。")}</small>}
      <div className={styles.bindings}>
        {!settings.bindings.length && <small className={styles.empty}>{t("还没有映射。添加一条映射后，Devin 才能请求模型库里的模型。")}</small>}
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
              <FormField label={t("Devin 模型")} hint={t("从 Devin 的模型列表里选；也可以直接填写标识。")}><Combobox value={binding.model_uid} options={uidOptions} onChange={(model_uid) => updateBinding(index, { model_uid })} /></FormField>
              <FormField label={t("模型库模型")}><Select value={binding.model_hash} options={modelOptions} ariaLabel={t("模型库模型")} onChange={(model_hash) => {
                const model = models.find((item) => item.model_hash === model_hash);
                updateBinding(index, { model_hash, display_name: model?.display_name ?? binding.display_name, context_window_tokens: model?.context_window_tokens ?? null });
              }} /></FormField>
              <div className={styles.bindingRowActions}>
                {!binding.enabled && <span className={styles.bindingPaused}>{t("已停用")}</span>}
                <button type="button" className={styles.more} aria-expanded={expanded} aria-label={t("{model} 的更多设置", { model: name })} onClick={() => setExpandedBindings((current) => ({ ...current, [rowKey]: !expanded }))}>
                  {expanded ? t("收起") : t("更多")}
                </button>
                <button type="button" className={styles.remove} aria-label={t("移除映射：{model}", { model: name })} onClick={() => removeBinding(index)}>{t("移除")}</button>
              </div>
            </div>
            {expanded && <div className={styles.bindingExtra}>
              <div className={styles.bindingFields}>
                <FormField label={t("显示名称")}><TextInput value={binding.display_name} onChange={(event) => updateBinding(index, { display_name: event.target.value })} /></FormField>
                <FormField label={t("上下文 token")}><TextInput type="number" min={1} value={binding.context_window_tokens ?? ""} onChange={(event) => updateBinding(index, { context_window_tokens: event.target.value ? Number(event.target.value) : null })} /></FormField>
                <FormField label={t("绑定类型")} hint={t("上下文压缩绑定必须是单线路；切换为该类型会清空候选路由。")}>
                  <Select value={binding.kind} options={kindOptions} ariaLabel={t("绑定类型")} onChange={(kind) => {
                    const nextKind = kind as DevinBindingKind;
                    updateBinding(index, nextKind === "context_compression" ? { kind: nextKind, routes: [], active_route_id: null } : { kind: nextKind });
                  }} />
                </FormField>
              </div>
              {isCompression ? <small className={styles.routeHint}>{t("上下文压缩绑定只使用上面的模型，不参与候选路由。")}</small> : <div className={styles.routes}>
                <div className={styles.routesHeader}>
                  <span>{t("候选路由")}</span>
                  <Button size="small" disabled={!models.length} onClick={() => appendRoute(index, binding, routes)}>{t("添加候选路由")}</Button>
                </div>
                {!hasRoutes && <small className={styles.routeHint}>{t("尚未配置候选路由，当前直接使用上面的主模型。")}</small>}
                {routes.map((route, routeIndex) => <div className={styles.route} key={route.route_id}>
                  <div className={styles.routeFields}>
                    <FormField label={t("路由模型")}><Select value={route.model_hash} options={modelOptions} ariaLabel={t("路由模型")} onChange={(model_hash) => updateRoute(index, binding, routes, routeIndex, { model_hash })} /></FormField>
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
    </TitledCard>
    <TitledCard
      title={t("接入 Devin")}
      description={t("改写 Devin 自己的配置文件，把它的请求指到本机网关")}
      badge={hostStatus?.patched
        ? <StatusPill tone="ok">{t("已打补丁")}</StatusPill>
        : hostStatus?.clean
          ? <StatusPill tone="idle">{t("尚未接入")}</StatusPill>
          : hostStatus
            ? <StatusPill tone="warn">{t("版本不匹配")}</StatusPill>
            : undefined}
      action={<div className={styles.hostActions}>
        <Button size="small" disabled={hostBusy} onClick={() => void inspectHost()}>{hostBusy ? t("检查中…") : t("检查宿主")}</Button>
        <Button size="small" variant="primary" disabled={!hostStatus?.clean || !saved.enabled || view.pendingChanges || hostBusy} onClick={() => void applyHostPatch()}>{t("应用补丁")}</Button>
        <Button size="small" disabled={!hostStatus?.restore_receipt || hostBusy} onClick={() => void restoreHostPatch()}>{t("恢复原文件")}</Button>
      </div>}
    >
      {/* 状态说明与「按钮为什么点不了」都在卡片正文里：禁用是唯一的反馈，原因不写出来
          就只剩一个灰按钮。 */}
      <div className={styles.hostNotes}>
        {hostStatus
          ? <p>{hostStatus.patched && hostStatus.ports
            /* 文件是打过补丁的，不代表它指向本应用当前的端口：另一个路由器留下的补丁
               也会让这里显示"已打补丁"。端口不一致时说清楚，别让"已接入"读成"接好了"。 */
            ? hostStatus.ports.api_port === saved.api_port
              ? t("已接入：API {api} · 推理 {inference} · 本地 API {local}", { api: hostStatus.ports.api_port, inference: hostStatus.ports.inference_port, local: hostStatus.ports.local_api_port })
              : t("宿主文件里的端口与当前设置不同（{api} · {inference} · {local}）：保存设置后重新应用补丁。", { api: hostStatus.ports.api_port, inference: hostStatus.ports.inference_port, local: hostStatus.ports.local_api_port })
            : hostStatus.patched
              ? clientErrorText(hostStatus.message)
              : hostStatus.clean ? t("兼容版本，尚未应用补丁") : clientErrorText(hostStatus.message)}</p>
          : <p role={hostFailure ? "alert" : undefined}>{hostFailure ? clientErrorText(hostFailure) : t("宿主接入只对 Devin 自己的 extension.js 操作，应用前会校验版本锚点并创建备份；路径已自动探测。")}</p>}
        {patchReason && <p>{patchReason}</p>}
        {restoreReason && <p>{restoreReason}</p>}
      </div>
    </TitledCard>
    <TitledCard
      title={t("高级")}
      description={t("端口、令牌与宿主文件路径；默认值适用于绝大多数情况")}
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
          <TextInput value={settings.upstream_api_url} onChange={(event) => update("upstream_api_url", event.target.value)} placeholder="https://server.self-serve.windsurf.com" />
        </FormField>
      </div>
      <p className={styles.note}>{t("网关固定监听 127.0.0.1，并限制单次请求体为 24 MiB。Devin 负责执行工具，本机网关负责模型调用和事件转发。")}</p>
      <div className={styles.fields}>
        <FormField label={t("Devin / Windsurf extension.js 路径")} hint={t("留空即自动探测；也可以填安装目录，或 extension.js 的完整路径。")}>
          <TextInput value={hostPath} onChange={(event) => rememberHostPath(event.target.value)} placeholder={t("留空自动探测")} />
        </FormField>
      </div>
    </TitledCard>
    {view.pendingChanges && <div className={styles.saveBar}>
      <span>{t("有未保存的改动")}</span>
      <Button variant="primary" size="small" disabled={saving} onClick={() => void save()}>{saving ? t("保存中…") : t("保存")}</Button>
    </div>}
  </div>;
  return <PageContent
    title={<PageTitle
      title="Devin"
      status={<StatusPill tone={view.summary.tone}>{view.summary.label}</StatusPill>}
      meta={t("把 Devin 的模型请求指到本机网关，由模型库回答")}
    />}
    sections={[{ key: "devin", estimatedHeight: 900, content }]}
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
