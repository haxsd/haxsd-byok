//! 接入状态的推导：状态面板回答的是"现在通没通"，所以它只读已保存的配置与
//! 服务端报告的监听状态。表单草稿只决定编辑与保存（`pendingChanges`），不得
//! 让一个还没保存的开关看起来已经生效。

import type { DevinHostStatus, DevinSettings, Model } from "../../shared/api";
import type { PathStage, PathState } from "../../shared/ui/ConnectionPath";
import type { StatusTone } from "../../shared/ui/StatusPill";
import { effectiveBindingModelHash } from "./devinBindingEdits";

export interface DevinConnectionInput {
  /** 表单草稿：只用来判断"有未保存的改动"。 */
  draft: DevinSettings;
  /** 服务端已保存的配置：所有状态结论的事实来源。 */
  saved: DevinSettings;
  models: Model[];
  hostStatus: DevinHostStatus | null;
  /** 服务端报告的网关监听状态；null 表示还没问到。 */
  gateway: { listening: boolean; ready: boolean } | null;
  devinCalls: number | null;
  /** 最近一次 Devin 调用是否成功；null 表示还没有调用或读不到。 */
  lastCallSuccess: boolean | null;
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
  key: "model" | "gateway" | "connect" | "verify";
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
    footnotes: string | undefined;
  };
}

export function devinConnectionView(input: DevinConnectionInput): DevinConnectionView {
  const { draft, saved, models, hostStatus, gateway: gatewayReading, devinCalls, lastCallSuccess } = input;
  const gatewayListening = gatewayReading?.listening ?? null;
  const gatewayReady = gatewayReading?.ready ?? false;

  const standard = saved.bindings.filter((binding) => binding.enabled && binding.kind !== "context_compression");
  const compression = saved.bindings.filter((binding) => binding.enabled && binding.kind === "context_compression");
  const defaultUid = saved.default_model_uid?.trim() || null;
  const defaultBinding = defaultUid
    ? standard.find((binding) => binding.model_uid.trim() === defaultUid) ?? null
    : null;
  const activeModel = defaultBinding
    ? models.find((model) => model.model_hash === effectiveBindingModelHash(defaultBinding)) ?? null
    : null;

  const savedEnabled = saved.enabled;
  const settingsConfigured = hostStatus?.configured ?? false;
  const extensionPatched = hostStatus?.extension.patched ?? false;
  const devinPointsHere = settingsConfigured
    && extensionPatched
    && hostStatus?.ports?.api_port === saved.api_port
    && hostStatus?.ports?.inference_port === saved.inference_port;
  const devinState: "here" | "settings-only" | "patch-only" | "elsewhere" | "clean" | "unknown" = !savedEnabled
    ? "unknown"
    : devinPointsHere
      ? "here"
      : settingsConfigured && !extensionPatched
        ? "settings-only"
        : extensionPatched && !settingsConfigured
          ? "patch-only"
          : settingsConfigured
            ? "elsewhere"
            : hostStatus?.unconfigured
              ? "clean"
              : "unknown";
  const devinLabel = {
    here: t("已指向本机网关"),
    "settings-only": t("设置已写入，缺兼容补丁"),
    "patch-only": t("兼容补丁已写入，缺服务地址"),
    elsewhere: t("已指向其他网关"),
    clean: t("尚未接管"),
    unknown: t("未知"),
  }[devinState];
  const devinTone: StatusTone = devinState === "here"
    ? "ok"
    : devinState === "settings-only" || devinState === "patch-only" || devinState === "clean"
      ? "warn"
      : "idle";

  const gateway: DevinGatewayState = {
    listening: gatewayListening,
    ready: savedEnabled && gatewayReady,
    pendingRestart: savedEnabled && gatewayReading !== null && !gatewayReady,
    tone: savedEnabled ? (gatewayReady ? "ok" : "warn") : gatewayListening === true ? "warn" : "idle",
    label: !savedEnabled
      ? gatewayListening === true
        ? t("已关闭（仍在监听，重启后停止）")
        : t("已关闭")
      : gatewayReading === null
        ? t("检查中…")
        : gatewayReady
          ? t("运行中")
          : gatewayListening
            ? t("端口已更改，重启后生效")
            : t("已启用，端口未监听"),
  };

  const gatewayStep: (DevinConnectionStep & { done: boolean }) | null = savedEnabled
    && gatewayReading !== null
    && !gatewayReady
    ? {
        key: "gateway",
        done: false,
        label: t("让本机网关就绪"),
        hint: gatewayListening
          ? t("端口已更改，重启后生效")
          : t("重启应用，让网关在保存的端口上监听"),
      }
    : null;
  const steps: (DevinConnectionStep & { done: boolean })[] = [
    {
      key: "model",
      done: savedEnabled && defaultBinding !== null && activeModel !== null,
      label: t("选择默认模型"),
      hint: !savedEnabled
        ? t("打开“使用自选模型”并选择模型")
        : !standard.length
          ? t("选择模型后保存")
          : !defaultUid
            ? t("选择默认模型后保存")
            : !defaultBinding
              ? t("默认模型已删除或已停用")
              : activeModel
                ? t("已选择 {model}", { model: activeModel.display_name })
                : t("模型已删除或不可用"),
    },
    ...(gatewayStep ? [gatewayStep] : []),
    {
      key: "connect",
      done: devinPointsHere,
      label: t("把 Devin 指到本机网关"),
      hint: devinPointsHere
        ? t("设置与兼容补丁均已写入")
        : devinState === "settings-only"
          ? t("设置已写入，但 Devin 仍不读取；重新写入接入配置")
          : devinState === "patch-only"
            ? t("兼容补丁已写入，重新写入服务地址")
            : t("用下面的接入设置完成"),
    },
    {
      key: "verify",
      done: lastCallSuccess === true,
      label: t("在 Devin 里跑一次对话"),
      hint: devinCalls === null
        ? t("无法读取调用记录")
        : lastCallSuccess === true
          ? t("最近一次调用成功（共 {count} 条）", { count: devinCalls })
          : devinCalls > 0
            ? t("最近一次调用未成功，请检查模型或重新测试")
          : t("还没有 Devin 调用记录"),
    },
  ];

  const stageState = (tone: StatusTone): PathState => tone === "ok" ? "up" : tone === "warn" ? "down" : "unknown";
  const stages: PathStage[] = [
    { key: "devin", label: "Devin", detail: devinLabel, state: stageState(devinTone) },
    {
      key: "gateway",
      label: t("本机网关"),
      detail: gateway.ready ? t("已运行") : gateway.label,
      state: stageState(gateway.tone),
    },
    {
      key: "model",
      label: t("模型库"),
      detail: activeModel
        ? activeModel.display_name
        : defaultUid
          ? defaultBinding
            ? t("模型已删除或哈希无效")
            : t("默认模型已删除或已停用")
          : standard.length
            ? t("未选择默认模型")
            : t("未绑定"),
      state: activeModel ? "up" : standard.length > 0 || defaultUid ? "down" : "unknown",
    },
  ];

  const outstanding = steps.filter((step) => !step.done);
  const connected = stages.every((stage) => stage.state === "up") && lastCallSuccess === true;
  const summary = {
    tone: connected ? "ok" as const : gateway.tone === "ok" ? "warn" as const : gateway.tone,
    label: connected
      ? t("已连接")
      : !savedEnabled
        ? gatewayListening === true
          ? t("已关闭（仍在监听，重启后停止）")
          : t("未启用")
        : gatewayReading === null
          ? t("检查中…")
          : t("待完成"),
  };
  return {
    pendingChanges: JSON.stringify(draft) !== JSON.stringify(saved),
    summary,
    gateway,
    hero: {
      connected,
      title: connected
        ? t("可以使用了")
        : outstanding.length
          ? t("还差 {count} 步", { count: outstanding.length })
          : t("正在检查连接…"),
      description: connected
        ? t("在 Devin 里直接对话，会由 {model} 回答。", { model: activeModel?.display_name ?? "" })
        : outstanding[0]?.label ?? t("检查中…"),
      stages,
      steps: outstanding.map(({ key, label, hint }) => ({ key, label, hint })),
      footnotes: compression.length ? `${t("上下文压缩绑定")}：${t("已配置 {count} 个", { count: compression.length })}` : undefined,
    },
  };
}
