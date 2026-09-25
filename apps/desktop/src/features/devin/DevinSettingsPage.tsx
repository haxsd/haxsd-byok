import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, type DevinBindingKind, type DevinHostPatchReceipt, type DevinHostPatchStatus, type DevinModelBinding, type DevinModelChoice, type DevinRoute, type DevinSettings, type Model } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { FormField, SecretTextInput, TextInput } from "../../shared/ui/FormControls";
import { Combobox, Select } from "../../shared/ui/Select";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import { StatusHero } from "../../shared/ui/StatusHero";
import type { PathStage } from "../../shared/ui/ConnectionPath";
import { PageContent } from "../../shell/layout/PageContent";
import { PageTitle } from "../../shared/ui/PageTitle";
import { StatusPill } from "../../shared/ui/StatusPill";
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
const hostPathStorageKey = "haxsd-byok.devin.host-path";

/** 读取上次记住的宿主路径；只在初始化时调用。 */
function readHostPath(): string {
  try {
    return localStorage.getItem(hostPathStorageKey) ?? "";
  } catch {
    // Storage may be unavailable; the field simply stays empty.
    return "";
  }
}

/** 记住宿主路径；持久化失败不影响当前会话。 */
function writeHostPath(path: string) {
  try {
    if (path.trim()) localStorage.setItem(hostPathStorageKey, path);
  } catch {
    // Persisting the path is best effort.
  }
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
  const used = new Set(bindings.map((binding) => binding.model_uid.trim()));
  const free = choices.find((choice) => !used.has(choice.uid));
  return (free ?? choices[0]).uid;
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
  const [hostReceipt, setHostReceipt] = useState<DevinHostPatchReceipt | null>(null);
  const [hostBusy, setHostBusy] = useState(false);
  // Which mapping rows are showing their optional fields. Collapsed by default: the
  // page should read as two decisions per mapping, not as a form of five fields.
  const [expandedBindings, setExpandedBindings] = useState<Record<string, boolean>>({});
  const [gatewayPortsUp, setGatewayPortsUp] = useState<boolean | null>(null);
  const [devinCalls, setDevinCalls] = useState<number | null>(null);

  useEffect(() => {
    void Promise.all([api.devinSettings(), api.models()]).then(([nextSettings, nextModels]) => {
      setSettings(nextSettings);
      setSaved(nextSettings);
      setModels(nextModels);
    }).catch((cause) => message.error(cause)).finally(() => setLoading(false));
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
  // 浏览器直接探测会被 CORS 拒绝，从而把健康的网关误报成"端口未监听"。
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
  }, [loading, settings.enabled]);

  // 宿主路径不再要求用户填写：服务端自己找到安装位置，再回填到界面。
  //
  // 这里刻意只跑一次，用的是"打开页面时记住的那条路径"：把 hostPath 列进依赖会让
  // 用户每敲一个字符就打一次探测请求。路径本身由 useState 的初始值给出，所以闭包里
  // 拿到的就是该用的值，而不是某个中间状态。
  useEffect(() => {
    if (loading) return;
    let cancelled = false;
    const remembered = hostPath.trim();
    void api.devinHostStatus(remembered || undefined)
      .then((status) => {
        if (cancelled) return;
        setHostStatus(status);
        if (!remembered) {
          setHostPath(status.path);
          writeHostPath(status.path);
        }
      })
      .catch((cause) => {
        // 探测失败要说出原因，不能静默成"未知"。
        if (!cancelled) message.error(cause);
      });
    return () => {
      cancelled = true;
    };
  }, [loading, message]);

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
    } catch (cause) {
      message.error(cause);
    } finally {
      setHostBusy(false);
    }
  };
  const applyHostPatch = async () => {
    try {
      setHostBusy(true);
      const receipt = await api.applyDevinHostPatch(hostPath);
      setHostReceipt(receipt);
      setHostStatus(await api.devinHostStatus(hostPath));
      message(t("Devin 宿主补丁已应用；重启 Devin 后生效"), { duration: 5_000 });
    } catch (cause) {
      message.error(cause);
    } finally {
      setHostBusy(false);
    }
  };
  const restoreHostPatch = async () => {
    if (!hostReceipt) return;
    try {
      setHostBusy(true);
      await api.restoreDevinHostPatch(hostReceipt);
      setHostReceipt(null);
      setHostStatus(await api.devinHostStatus(hostPath));
      message(t("Devin 宿主文件已恢复"));
    } catch (cause) {
      message.error(cause);
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
      message.error(cause);
    } finally {
      setSaving(false);
    }
  };
  const dirty = JSON.stringify(settings) !== JSON.stringify(saved);
  const gatewayReady = settings.enabled && gatewayPortsUp === true;
  const gatewaySummary = !settings.enabled ? t("未启用") : gatewayReady ? t("运行中") : gatewayPortsUp === null ? t("检查中…") : t("待重启");

  const modelOptions = models.map((model) => ({ value: model.model_hash, label: `${model.display_name} · ${model.model_hash.slice(0, 8)}` }));
  // 选择器显示 Devin 里的模型名，写回的仍是客户端随后会请求的标识。
  const uidOptions = modelChoices.map((choice) => ({ value: choice.uid, label: choice.name }));
  const kindOptions = [
    { value: "standard" as DevinBindingKind, label: t("标准") },
    { value: "context_compression" as DevinBindingKind, label: t("上下文压缩") },
  ];
  const content = loading ? <div className={styles.loading}>{t("加载中…")}</div> : <div className={styles.page}>
    {(() => {
      const standard = settings.bindings.filter((binding) => binding.enabled && binding.kind !== "context_compression");
      const compression = settings.bindings.filter((binding) => binding.enabled && binding.kind === "context_compression");
      const activeBinding = standard[0] ?? null;
      const activeHash = activeBinding
        ? (activeBinding.active_route_id
          ? activeBinding.routes.find((route) => route.route_id === activeBinding.active_route_id && route.enabled)?.model_hash ?? activeBinding.model_hash
          : activeBinding.model_hash)
        : "";
      const activeModel = models.find((model) => model.model_hash === activeHash) ?? null;
      const devinPatched = hostStatus?.patched ?? false;
      const devinPointsHere = devinPatched && hostStatus?.ports?.api_port === settings.api_port;
      const devinPatchedElsewhere = devinPatched && !devinPointsHere;
      const gatewayLabel = !settings.enabled
        ? t("已关闭")
        : gatewayPortsUp === null
          ? t("检查中…")
          : gatewayPortsUp
            ? t("运行中")
            : t("已启用，端口未监听");
      const gatewayTone = settings.enabled && gatewayPortsUp === true ? "ok" : settings.enabled ? "warn" : "idle";
      const devinLabel = !settings.enabled
        ? t("未知")
        : devinPointsHere
          ? t("已指向本机网关")
          : devinPatchedElsewhere
            ? t("已指向其他路由器")
            : hostStatus?.clean
              ? t("尚未接入")
              : t("未知");
      const devinTone = devinPointsHere ? "ok" : devinPatchedElsewhere || devinLabel === t("未知") ? "idle" : "warn";
      const steps = [
        {
          key: "enable",
          done: settings.enabled && gatewayPortsUp === true,
          label: t("启用 Devin 网关"),
          hint: settings.enabled
            ? gatewayPortsUp === true
              ? t("端口已就绪")
              : t("已保存，重启应用后端口才会打开")
            : t("打开上面的开关并保存"),
        },
        {
          key: "bind",
          done: standard.length > 0,
          label: t("绑定一个模型"),
          hint: standard.length > 0 ? t("已绑定 {count} 个", { count: standard.length }) : t("还没有映射"),
        },
        {
          key: "connect",
          done: devinPointsHere,
          label: t("把 Devin 指到本机网关"),
          hint: devinPointsHere ? t("已接入") : t("用下面的宿主接入完成"),
        },
        {
          key: "verify",
          done: (devinCalls ?? 0) > 0,
          label: t("在 Devin 里跑一次对话"),
          hint: devinCalls === null
            ? t("无法读取调用记录")
            : devinCalls > 0
              ? t("已有 {count} 条 Devin 调用记录", { count: devinCalls })
              : t("还没有 Devin 调用记录"),
        },
      ];
      const stages: PathStage[] = [
        {
          key: "devin",
          label: "Devin",
          detail: devinLabel,
          state: devinPointsHere ? "up" : devinTone === "warn" ? "down" : "unknown",
        },
        {
          key: "gateway",
          label: t("本机网关"),
          // The ports are the useful detail only while it is listening; otherwise the
          // reason it is not carries more information.
          detail: gatewayTone === "ok"
            ? `${settings.api_port} · ${settings.inference_port} · ${settings.local_api_port}`
            : gatewayLabel,
          state: gatewayTone === "ok" ? "up" : gatewayTone === "warn" ? "down" : "unknown",
        },
        {
          key: "model",
          label: t("模型库"),
          detail: activeModel ? activeModel.display_name : standard.length ? t("模型已删除或哈希无效") : t("未绑定"),
          state: activeModel ? "up" : standard.length ? "down" : "unknown",
        },
      ];
      // Only what is still missing stays on screen. A checklist that keeps showing
      // four ticks after everything works is noise on every later visit.
      const outstanding = steps.filter((step) => !step.done);
      const connected = stages.every((stage) => stage.state === "up");
      return <TitledCard title={t("接入状态")} action={<Button size="small" onClick={() => navigate("/calls")}>{t("查看调用记录")}</Button>}>
        <StatusHero
          connected={connected}
          title={connected ? t("已接通") : t("还差 {count} 步", { count: outstanding.length })}
          description={connected
            ? t("Devin 的请求正在走本机网关，由 {model} 回答。", { model: activeModel?.display_name ?? "" })
            : outstanding[0]?.label ?? ""}
          stages={stages}
          steps={outstanding}
          footnotes={<>{t("上下文压缩绑定")}：{compression.length ? t("已配置 {count} 个", { count: compression.length }) : t("未配置")}</>}
        />
      </TitledCard>;
    })()}
    <TitledCard
      title={t("基础设置")}
      description={t("网关开关与模型映射")}
      collapsible={false}
    >
      <div className={styles.settingRow}>
        <div><strong>{t("启用 Devin 网关")}</strong><small>{t("关闭时不会打开任何 Devin 端口，也不会影响 Cursor。")}</small></div>
        <Switch checked={settings.enabled} label={t("启用 Devin 网关")} onChange={(enabled) => update("enabled", enabled)} />
      </div>
      <div className={styles.settingRow}>
        <div><strong>{t("模型映射")}</strong><small>{t("把 Devin 的模型标识指向模型库里的模型；Devin 只会用到这里配置的映射。")}</small></div>
        <Button size="small" disabled={!models.length} onClick={addBinding}>{t("添加映射")}</Button>
      </div>
      <div className={styles.bindings}>
        {!settings.bindings.length && <small className={styles.empty}>{t("还没有映射。添加一个 haxsd byok 模型后，Devin 才能使用它。")}</small>}
        {settings.bindings.map((binding, index) => {
          const routes = routesForDisplay(binding);
          const activeRouteId = activeRouteIdForDisplay(binding);
          const hasRoutes = binding.routes.length > 0;
          const isCompression = binding.kind === "context_compression";
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
              <FormField label={t("haxsd byok 模型")}><Select value={binding.model_hash} options={modelOptions} ariaLabel={t("haxsd byok 模型")} onChange={(model_hash) => {
                const model = models.find((item) => item.model_hash === model_hash);
                updateBinding(index, { model_hash, display_name: model?.display_name ?? binding.display_name, context_window_tokens: model?.context_window_tokens ?? null });
              }} /></FormField>
              <div className={styles.bindingRowActions}>
                {!binding.enabled && <span className={styles.bindingPaused}>{t("已停用")}</span>}
                <button type="button" className={styles.more} aria-expanded={expanded} onClick={() => setExpandedBindings((current) => ({ ...current, [rowKey]: !expanded }))}>
                  {expanded ? t("收起") : t("更多")}
                </button>
                <button type="button" className={styles.remove} onClick={() => removeBinding(index)}>{t("移除")}</button>
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
                    <Switch checked={route.enabled} label={t("启用此路由")} onChange={(enabled) => updateRoute(index, binding, routes, routeIndex, { enabled })} />
                    <div className={styles.routeActions}>
                      <Button size="small" variant={route.route_id === activeRouteId ? "primary" : "secondary"} disabled={!route.enabled || route.route_id === activeRouteId} onClick={() => commitRoutes(index, routes, route.route_id)}>{route.route_id === activeRouteId ? t("当前路由") : t("设为当前路由")}</Button>
                      <button type="button" className={styles.remove} disabled={!hasRoutes} onClick={() => removeRoute(index, binding, routes, routeIndex)}>{t("移除")}</button>
                    </div>
                  </div>
                </div>)}
              </div>}
              <div className={styles.bindingFooter}>
                <Switch checked={binding.enabled} label={t("启用此模型映射")} onChange={(enabled) => updateBinding(index, { enabled })} />
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
        <Button size="small" variant="primary" disabled={!hostStatus?.clean || !settings.enabled || hostBusy} onClick={() => void applyHostPatch()}>{t("应用补丁")}</Button>
        <Button size="small" disabled={!hostReceipt || hostBusy} onClick={() => void restoreHostPatch()}>{t("恢复原文件")}</Button>
      </div>}
    >
      {hostStatus
        ? <p className={styles.note}>{hostStatus.patched && hostStatus.ports
          ? t("已接入：API {api} · 推理 {inference} · Local API {local}", { api: hostStatus.ports.api_port, inference: hostStatus.ports.inference_port, local: hostStatus.ports.local_api_port })
          : hostStatus.patched
            ? hostStatus.message
            : hostStatus.clean ? t("兼容版本，尚未应用补丁") : hostStatus.message}</p>
        : <p className={styles.note}>{t("宿主接入只对 Devin 自己的 extension.js 操作，应用前会校验版本锚点并创建备份；路径已自动探测。")}</p>}
    </TitledCard>
    <TitledCard
      title={t("高级")}
      description={t("端口、令牌与宿主文件路径；默认值适用于绝大多数情况")}
      collapsible
      storageKey="devin-advanced"
    >
      <div className={styles.fields}>
        <FormField label={t("控制令牌")} hint={t("可选；Devin 请求可通过 x-devin-router-token 或 Bearer 令牌认证。")}>
          <SecretTextInput value={settings.auth_token} onChange={(event) => update("auth_token", event.target.value)} placeholder={t("留空表示仅依赖本机回环访问")} />
        </FormField>
        <FormField label={t("API 端口")}><TextInput type="number" value={settings.api_port} onChange={(event) => update("api_port", Number(event.target.value))} /></FormField>
        <FormField label={t("推理端口")}><TextInput type="number" value={settings.inference_port} onChange={(event) => update("inference_port", Number(event.target.value))} /></FormField>
        <FormField label={t("本地 API 端口")}><TextInput type="number" value={settings.local_api_port} onChange={(event) => update("local_api_port", Number(event.target.value))} /></FormField>
        <FormField label={t("上游 API 地址")} hint={t("登录、账号与遥测等本地不处理的方法会转发到这里；模型请求不经过它。")}>
          <TextInput value={settings.upstream_api_url} onChange={(event) => update("upstream_api_url", event.target.value)} placeholder="https://server.self-serve.windsurf.com" />
        </FormField>
      </div>
      <p className={styles.note}>{t("网关固定监听 127.0.0.1，并限制单次请求体为 24 MiB。Devin 负责执行工具，haxsd byok 负责模型调用和事件转发。")}</p>
      <div className={styles.fields}>
        <FormField label={t("Devin / Windsurf extension.js 路径")} hint={t("留空即自动探测；也可以填安装目录，或 extension.js 的完整路径。")}>
          <TextInput value={hostPath} onChange={(event) => rememberHostPath(event.target.value)} placeholder={t("留空自动探测")} />
        </FormField>
      </div>
    </TitledCard>
    {dirty && <div className={styles.saveBar}>
      <span>{t("有未保存的改动")}</span>
      <Button variant="primary" size="small" disabled={saving} onClick={() => void save()}>{saving ? t("保存中…") : t("保存")}</Button>
    </div>}
  </div>;
  return <PageContent
    title={<PageTitle
      title="Devin"
      status={<StatusPill tone={gatewayReady ? "ok" : settings.enabled ? "warn" : "idle"}>{gatewaySummary}</StatusPill>}
      meta={t("把 Devin 的模型请求指到本机网关，由模型库回答")}
    />}
    sections={[{ key: "devin", estimatedHeight: 900, content }]}
  />;
}
