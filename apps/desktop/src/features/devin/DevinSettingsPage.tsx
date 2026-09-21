import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, type DevinBindingKind, type DevinHostPatchReceipt, type DevinHostPatchStatus, type DevinModelBinding, type DevinRoute, type DevinSettings, type Model } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { FormField, SecretTextInput, TextInput } from "../../shared/ui/FormControls";
import { Select } from "../../shared/ui/Select";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import { PageContent } from "../../shell/layout/PageContent";
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

export function DevinSettingsPage() {
  const message = useMessage();
  const navigate = useNavigate();
  const [settings, setSettings] = useState<DevinSettings>(emptySettings);
  const [models, setModels] = useState<Model[]>([]);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [hostPath, setHostPath] = useState("");
  const [hostStatus, setHostStatus] = useState<DevinHostPatchStatus | null>(null);
  const [hostReceipt, setHostReceipt] = useState<DevinHostPatchReceipt | null>(null);
  const [hostBusy, setHostBusy] = useState(false);
  const [gatewayPortsUp, setGatewayPortsUp] = useState<boolean | null>(null);
  const [devinCalls, setDevinCalls] = useState<number | null>(null);

  useEffect(() => {
    try {
      const stored = localStorage.getItem(hostPathStorageKey);
      if (stored) setHostPath(stored);
    } catch {
      // Storage may be unavailable; the field simply stays empty.
    }
  }, []);

  useEffect(() => {
    void Promise.all([api.devinSettings(), api.models()]).then(([nextSettings, nextModels]) => {
      setSettings(nextSettings);
      setModels(nextModels);
    }).catch((cause) => message(cause instanceof Error ? cause.message : String(cause))).finally(() => setLoading(false));
  }, [message]);

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
  useEffect(() => {
    if (loading) return;
    let cancelled = false;
    void api.devinHostStatus(hostPath.trim() || undefined)
      .then((status) => {
        if (cancelled) return;
        setHostStatus(status);
        if (!hostPath.trim()) {
          setHostPath(status.path);
          try {
            localStorage.setItem(hostPathStorageKey, status.path);
          } catch {
            // Persisting the path is best effort.
          }
        }
      })
      .catch((cause) => {
        // 探测失败要说出原因，不能静默成"未知"。
        if (!cancelled) message(cause instanceof Error ? cause.message : String(cause));
      });
    return () => {
      cancelled = true;
    };
  }, [loading, message]);

  const rememberHostPath = (value: string) => {
    setHostPath(value);
    setHostStatus(null);
    try {
      if (value.trim()) localStorage.setItem(hostPathStorageKey, value);
    } catch {
      // Persisting the path is best effort.
    }
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
      model_uid: `cursor-byok-${settings.bindings.length + 1}`,
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
      message(cause instanceof Error ? cause.message : String(cause));
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
      message("Devin 宿主补丁已应用；重启 Devin 后生效", { duration: 5_000 });
    } catch (cause) {
      message(cause instanceof Error ? cause.message : String(cause));
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
      message("Devin 宿主文件已恢复");
    } catch (cause) {
      message(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setHostBusy(false);
    }
  };
  const save = async () => {
    try {
      setSaving(true);
      const saved = await api.setDevinSettings(settings);
      setSettings(saved);
      message(t("Devin 设置已保存，重启软件后监听端口生效"), { duration: 5_000 });
    } catch (cause) {
      message(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSaving(false);
    }
  };

  const modelOptions = models.map((model) => ({ value: model.model_hash, label: `${model.display_name} · ${model.model_hash.slice(0, 8)}` }));
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
      return <TitledCard title={t("接入状态")} action={<Button size="small" onClick={() => navigate("/calls")}>{t("查看调用记录")}</Button>}>
        <div className={styles.status}>
          <div className={styles.statusHead}>
            <div className={styles.statusItems}>
              <div className={styles.statusItem}>
                <span>{t("网关")}</span>
                <strong><span className={`${styles.badge} ${gatewayTone === "ok" ? styles.badgeOk : gatewayTone === "warn" ? styles.badgeWarn : styles.badgeIdle}`}>{gatewayLabel}</span></strong>
              </div>
              <div className={styles.statusItem}><span>{t("端口")}</span><strong>{settings.api_port} / {settings.inference_port} / {settings.local_api_port}</strong></div>
              <div className={styles.statusItem}>
                <span>Devin</span>
                <strong><span className={`${styles.badge} ${devinTone === "ok" ? styles.badgeOk : devinTone === "warn" ? styles.badgeWarn : styles.badgeIdle}`}>{devinLabel}</span></strong>
              </div>
              <div className={styles.statusItem}>
                <span>{t("当前生效模型")}</span>
                <strong>{activeModel ? activeModel.display_name : standard.length ? t("模型已删除或哈希无效") : t("未绑定")}</strong>
              </div>
              <div className={styles.statusItem}><span>{t("上下文压缩绑定")}</span><strong>{compression.length ? t("已配置 {count} 个", { count: compression.length }) : t("未配置")}</strong></div>
            </div>
          </div>
          <div className={styles.steps}>
            {steps.map((step, index) => <div key={step.key} className={styles.step}>
              <span className={`${styles.stepMark} ${step.done ? styles.stepMarkDone : ""}`}>{step.done ? "✓" : index + 1}</span>
              <span className={styles.stepText}><strong>{step.label}</strong><small>{step.hint}</small></span>
              <span />
            </div>)}
          </div>
        </div>
      </TitledCard>;
    })()}
    <TitledCard title={t("基础设置")} collapsible={false}>
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
          return <div className={styles.binding} key={`${binding.model_uid}-${index}`}>
            <div className={styles.bindingFields}>
              <FormField label={t("Devin 模型 UID")}><TextInput value={binding.model_uid} onChange={(event) => updateBinding(index, { model_uid: event.target.value })} /></FormField>
              <FormField label={t("haxsd byok 模型")}><Select value={binding.model_hash} options={modelOptions} ariaLabel={t("haxsd byok 模型")} onChange={(model_hash) => {
                const model = models.find((item) => item.model_hash === model_hash);
                updateBinding(index, { model_hash, display_name: model?.display_name ?? binding.display_name, context_window_tokens: model?.context_window_tokens ?? null });
              }} /></FormField>
              <FormField label={t("显示名称")}><TextInput value={binding.display_name} onChange={(event) => updateBinding(index, { display_name: event.target.value })} /></FormField>
              <FormField label={t("上下文 token") }><TextInput type="number" min={1} value={binding.context_window_tokens ?? ""} onChange={(event) => updateBinding(index, { context_window_tokens: event.target.value ? Number(event.target.value) : null })} /></FormField>
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
              <button type="button" className={styles.remove} onClick={() => removeBinding(index)}>{t("移除")}</button>
            </div>
          </div>;
        })}
      </div>
    </TitledCard>
    <TitledCard title={t("高级")} collapsible>
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
      <p className={styles.note}>{t("宿主接入只对这里显示的 extension.js 操作。应用补丁前会校验四个版本锚点并创建 SHA-256 备份；未知版本、部分补丁或备份不一致时会拒绝写入。路径已自动探测，通常无需修改。")}</p>
      <div className={styles.fields}>
        <FormField label="Devin / Windsurf extension.js 路径" hint={t("留空即自动探测；仅在自动结果不正确时才需要填写。")}>
          <TextInput value={hostPath} onChange={(event) => rememberHostPath(event.target.value)} placeholder={t("留空自动探测")} />
        </FormField>
      </div>
      <div className={styles.hostActions}>
        <Button size="small" disabled={hostBusy} onClick={() => void inspectHost()}>{hostBusy ? t("检查中…") : t("检查宿主")}</Button>
        <Button size="small" variant="primary" disabled={!hostStatus?.clean || !settings.enabled || hostBusy} onClick={() => void applyHostPatch()}>{t("应用补丁")}</Button>
        <Button size="small" disabled={!hostReceipt || hostBusy} onClick={() => void restoreHostPatch()}>{t("恢复原文件")}</Button>
      </div>
      {hostStatus && <small className={styles.hostStatus}>{hostStatus.patched && hostStatus.ports
        ? t("已接入：API {api} · 推理 {inference} · Local API {local}", { api: hostStatus.ports.api_port, inference: hostStatus.ports.inference_port, local: hostStatus.ports.local_api_port })
        : hostStatus.patched
          ? hostStatus.message
          : hostStatus.clean ? t("兼容版本，尚未应用补丁") : hostStatus.message}</small>}
      <div className={styles.bindingFooter}>
        <span />
        <Button variant="primary" size="small" disabled={saving} onClick={() => void save()}>{saving ? t("保存中…") : t("保存")}</Button>
      </div>
    </TitledCard>
  </div>;
  return <PageContent title="Devin" sections={[{ key: "devin", estimatedHeight: 900, content }]} />;
}
