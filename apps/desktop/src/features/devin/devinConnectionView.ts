//! 接入状态的推导：状态面板回答的是"现在通没通"，所以它只读已保存的配置与
//! 服务端报告的监听状态。表单草稿只决定编辑与保存（`pendingChanges`），不得
//! 让一个还没保存的开关看起来已经生效。

import type { DevinHostPatchStatus, DevinSettings, Model } from "../../shared/api";
import type { PathStage, PathState } from "../../shared/ui/ConnectionPath";
import type { StatusTone } from "../../shared/ui/StatusPill";

export interface DevinConnectionInput {
  /** 表单草稿：只用来判断"有未保存的改动"。 */
  draft: DevinSettings;
  /** 服务端已保存的配置：所有状态结论的事实来源。 */
  saved: DevinSettings;
  models: Model[];
  hostStatus: DevinHostPatchStatus | null;
  /** 服务端报告的网关监听状态；null 表示还没问到。 */
  gatewayListening: boolean | null;
  devinCalls: number | null;
}

export interface DevinGatewayState {
  tone: StatusTone;
  label: string;
  /** 已保存开启，且端口确实在监听。 */
  ready: boolean;
  /** 已保存开启，但端口还没监听：重启应用后才会生效。 */
  pendingRestart: boolean;
  listening: boolean | null;
}

export interface DevinConnectionStep {
  key: "enable" | "bind" | "connect" | "verify";
  label: string;
  hint: string;
}

export interface DevinConnectionView {
  /** 草稿与已保存配置不同：界面据此提示"有未保存的改动"。 */
  pendingChanges: boolean;
  /** 页面标题右侧的状态。 */
  summary: { tone: StatusTone; label: string };
  gateway: DevinGatewayState;
  hero: {
    connected: boolean;
    title: string;
    description: string;
    stages: PathStage[];
    /** 还没完成的步骤；全部完成时为空。 */
    steps: DevinConnectionStep[];
    footnotes: string;
  };
}

/** 主映射当前生效的模型哈希：有候选路由时取选中的那条。 */
function activeModelHash(binding: DevinSettings["bindings"][number]): string {
  if (!binding.active_route_id) return binding.model_hash;
  const route = binding.routes.find((candidate) => candidate.route_id === binding.active_route_id && candidate.enabled);
  return route?.model_hash ?? binding.model_hash;
}

export function devinConnectionView(input: DevinConnectionInput): DevinConnectionView {
  const { draft, saved, models, hostStatus, gatewayListening, devinCalls } = input;

  const standard = saved.bindings.filter((binding) => binding.enabled && binding.kind !== "context_compression");
  const compression = saved.bindings.filter((binding) => binding.enabled && binding.kind === "context_compression");
  const activeBinding = standard[0] ?? null;
  const activeModel = activeBinding
    ? models.find((model) => model.model_hash === activeModelHash(activeBinding)) ?? null
    : null;

  const savedEnabled = saved.enabled;
  const patched = hostStatus?.patched ?? false;
  const devinPointsHere = patched && hostStatus?.ports?.api_port === saved.api_port;
  const devinState: "here" | "elsewhere" | "clean" | "unknown" = !savedEnabled
    ? "unknown"
    : devinPointsHere
      ? "here"
      : patched
        ? "elsewhere"
        : hostStatus?.clean
          ? "clean"
          : "unknown";
  const devinLabel = {
    here: t("已指向本机网关"),
    elsewhere: t("已指向其他路由器"),
    clean: t("尚未接入"),
    unknown: t("未知"),
  }[devinState];
  const devinTone: StatusTone = devinState === "here" ? "ok" : devinState === "clean" ? "warn" : "idle";

  const gateway: DevinGatewayState = {
    listening: gatewayListening,
    ready: savedEnabled && gatewayListening === true,
    pendingRestart: savedEnabled && gatewayListening === false,
    tone: savedEnabled ? (gatewayListening === true ? "ok" : "warn") : gatewayListening === true ? "warn" : "idle",
    label: !savedEnabled
      ? gatewayListening === true
        ? t("已关闭（仍在监听，重启后停止）")
        : t("已关闭")
      : gatewayListening === null
        ? t("检查中…")
        : gatewayListening
          ? t("运行中")
          : t("已启用，端口未监听"),
  };
  const summary = {
    tone: gateway.tone,
    label: !savedEnabled
      ? gatewayListening === true
        ? t("已关闭（仍在监听，重启后停止）")
        : t("未启用")
      : gateway.ready
        ? t("运行中")
        : gatewayListening === null
          ? t("检查中…")
          : t("待重启"),
  };

  const steps: (DevinConnectionStep & { done: boolean })[] = [
    {
      key: "enable",
      done: gateway.ready,
      label: t("启用 Devin 网关"),
      hint: savedEnabled
        ? gateway.ready
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

  const stageState = (tone: StatusTone): PathState => tone === "ok" ? "up" : tone === "warn" ? "down" : "unknown";
  const stages: PathStage[] = [
    { key: "devin", label: "Devin", detail: devinLabel, state: stageState(devinTone) },
    {
      key: "gateway",
      label: t("本机网关"),
      // 端口只在它确实在听时才有意义；其余时候"为什么没在听"信息量更大。
      detail: gateway.ready
        ? `${saved.api_port} · ${saved.inference_port} · ${saved.local_api_port}`
        : gateway.label,
      state: stageState(gateway.tone),
    },
    {
      key: "model",
      label: t("模型库"),
      detail: activeModel ? activeModel.display_name : standard.length ? t("模型已删除或哈希无效") : t("未绑定"),
      state: activeModel ? "up" : standard.length ? "down" : "unknown",
    },
  ];

  const outstanding = steps.filter((step) => !step.done);
  const connected = stages.every((stage) => stage.state === "up");
  return {
    pendingChanges: JSON.stringify(draft) !== JSON.stringify(saved),
    summary,
    gateway,
    hero: {
      connected,
      title: connected ? t("已接通") : t("还差 {count} 步", { count: outstanding.length }),
      description: connected
        ? t("Devin 的请求正在走本机网关，由 {model} 回答。", { model: activeModel?.display_name ?? "" })
        : outstanding[0]?.label ?? "",
      stages,
      steps: outstanding.map(({ key, label, hint }) => ({ key, label, hint })),
      footnotes: `${t("上下文压缩绑定")}：${compression.length ? t("已配置 {count} 个", { count: compression.length }) : t("未配置")}`,
    },
  };
}
