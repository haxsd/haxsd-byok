import type {
  ClaudeCliTakeoverState,
  ClaudeDesktopTakeoverState,
  CursorMode,
} from "../../shared/api";

/**
 * 教程的上手清单。
 *
 * 清单跟着用户在页面顶部选择的接入模块走：只有选定的那个模块会出现在
 * 步骤列表和进度分母里。没被选中的模块不渲染、不计入未完成——它是另一条
 * 路径，不是一个欠下的待办。选择只存放在页面状态里，不落盘。
 */

export type OnboardingClient = "cursor" | "devin" | "codex" | "claude";

export interface CursorOnboardingInput {
  caReady: boolean;
  settingsApplied: boolean;
  mode: CursorMode;
}

export interface DevinOnboardingInput {
  enabled: boolean;
  listening: boolean;
  ready: boolean;
}
export interface CodexOnboardingInput {
  enabled: boolean;
  listening: boolean;
  configured: boolean;
}

export interface ClaudeOnboardingInput {
  enabled: boolean;
  listening: boolean;
  cli: ClaudeCliTakeoverState;
  desktop: ClaudeDesktopTakeoverState;
}

export interface OnboardingInput {
  client: OnboardingClient | null;
  modelCount: number;
  cursor: CursorOnboardingInput | null;
  devin: DevinOnboardingInput | null;
  codex: CodexOnboardingInput | null;
  claude: ClaudeOnboardingInput | null;
  hostConfigured: boolean;
  /**
   * Devin 设置状态的读取过程。
   *
   * 读不到时 `hostConfigured` 会是 false，而那和「确实没写入」是两件事：前者会让
   * 用户重复写一次 Devin 设置。读取中/读取失败要各自说出来。
   */
  hostConfigRead?: "reading" | "failed" | null;
  callCounts: Partial<Record<OnboardingClient, number>>;
}

export interface OnboardingStep {
  key: "model" | "cursor" | "devin" | "codex" | "claude" | "verify";
  title: string;
  detail: string;
  status: string;
  done: boolean;
  path: string;
  action: string;
}

export interface OnboardingView {
  steps: OnboardingStep[];
  completed: number;
  total: number;
}

/** 客户端名是固定产品名，不参与翻译；步骤文案里需要与清单选择一致。 */
const clientNames: Record<OnboardingClient, string> = {
  cursor: "Cursor",
  devin: "Devin",
  codex: "Codex",
  claude: "Claude",
};

function modelStep(modelCount: number): OnboardingStep {
  return {
    key: "model",
    title: t("添加一个模型"),
    detail: t("模型库里的一项配置就是一个可用的模型：服务器地址、API Key、模型 ID。接入模块共用这一份。"),
    done: modelCount > 0,
    status: modelCount > 0 ? t("已有 {count} 个模型", { count: modelCount }) : t("还没有模型，任何客户端都无从回答"),
    path: "/models",
    action: t("去模型库"),
  };
}

/** 接管只有在「模式已离开官方、证书可用、代理设置已写入」三件事都成立时才算完成。 */
function cursorStep(cursor: CursorOnboardingInput | null): OnboardingStep {
  const takesOver = cursor !== null && cursor.mode !== "official";
  const caReady = cursor?.caReady ?? false;
  const settingsApplied = cursor?.settingsApplied ?? false;
  const status = !takesOver
    ? t("当前是官方模式，本应用不介入")
    : !caReady
      ? t("本地 CA 还没就绪")
      : !settingsApplied
        ? t("还差在 Cursor 页开启接管")
        : t("已接管");
  return {
    key: "cursor",
    title: t("让 Cursor 用上它"),
    detail: t("三件事：初始化本地 CA 并把证书装进系统信任库（用来读取 Cursor 的 HTTPS 请求）、在 Cursor 页选择模型来源（本地只用自己的模型，混合还能同时用官方模型）、再重启一次 Cursor。"),
    done: takesOver && caReady && settingsApplied,
    status,
    path: "/harness/cursor",
    action: t("去 Cursor 页"),
  };
}

/** 网关已就绪、Devin 设置已指向本机端口，且扩展读取开关已放开，Devin 这条路才算接通。 */
function devinStep(devin: DevinOnboardingInput | null, hostConfigured: boolean, hostConfigRead: OnboardingInput["hostConfigRead"]): OnboardingStep {
  const enabled = devin?.enabled ?? false;
  const listening = devin?.listening ?? false;
  const ready = devin?.ready ?? false;
  // 网关本身的状态优先：它没启用时，设置状态读没读到都不改变下一步。
  const status = !enabled
    ? t("网关未启用")
    : !ready
      ? listening
        ? t("端口已更改，重启后生效")
        : t("网关已启用，等待端口就绪")
      : hostConfigRead === "failed"
        ? t("读取失败")
        : hostConfigRead === "reading"
          ? t("读取中…")
          : hostConfigured
            ? t("已接通")
            : t("网关已就绪，还差写入 Devin 设置");
  return {
    key: "devin",
    title: t("让 Devin 用上它"),
    detail: t("四件事：选择默认模型、启用本机网关、把 Devin 的模型标识绑定到模型库、写入本机服务地址与兼容补丁。绑定之后 Devin 才认识这些模型；Devin 不在默认位置时，在「高级」里填写安装目录。"),
    done: enabled && ready && hostConfigured,
    status,
    path: "/harness/devin",
    action: t("去 Devin 页"),
  };
}

/** 网关在跑、且 config.toml 已写入本机路由，Codex 这条路才算接通。 */
function codexStep(codex: CodexOnboardingInput | null): OnboardingStep {
  const enabled = codex?.enabled ?? false;
  const listening = codex?.listening ?? false;
  const configured = codex?.configured ?? false;
  const status = !enabled
    ? t("网关未启用")
    : !listening
      ? t("网关已启用，等待端口就绪")
      : configured
        ? t("已接管")
        : t("网关在运行，还差应用配置");
  return {
    key: "codex",
    title: t("让 Codex 用上它"),
    detail: t("在「接入」里切到 Codex：选好可用模型与默认模型，保存后点「应用到 Codex」写入 config.toml，再重新打开客户端或新建一个本地会话。"),
    done: enabled && listening && configured,
    status,
    path: "/harness/codex",
    action: t("去 Codex 页"),
  };
}

/**
 * Claude 有 CLI 与 Desktop 两个客户端，任一接管成功都算这条路能用。
 *
 * 冲突和过期优先于「已接管」：一个客户端已经接通、另一个仍处于冲突时，不能把整条
 * 路径显示成已完成，否则用户会以为不需要处理另一个客户端。
 */
function claudeStep(claude: ClaudeOnboardingInput | null): OnboardingStep {
  const states = claude ? [claude.cli, claude.desktop] : [];
  const conflict = states.includes("conflict");
  const stale = states.includes("stale");
  const applied = states.includes("applied");
  const managed = states.includes("managed");
  const running = states.includes("running");
  const status = !claude
    ? t("读取中…")
    : conflict
      ? t("配置冲突")
      : stale
        ? t("配置已过期")
        : !claude.enabled
          ? t("网关未启用")
          : !claude.listening
            ? t("网关已启用，等待端口就绪")
            : applied
              ? t("已接管")
              : running
                ? t("先完全退出 Claude Desktop")
                : managed
                  ? t("受管配置，按页面指引处理")
                  : t("网关在运行，还差应用接管");
  return {
    key: "claude",
    title: t("让 Claude 用上它"),
    detail: t("选择默认模型并保存、启用 Claude 网关，再接管 Claude Code CLI 或 Claude Desktop；Desktop 接管前需要完全退出。"),
    done: Boolean(claude?.enabled && claude.listening && applied && !conflict && !stale),
    status,
    path: "/harness/claude",
    action: t("去 Claude 页"),
  };
}

function verifyStep(client: OnboardingClient, callCount: number): OnboardingStep {
  return {
    key: "verify",
    title: t("在客户端里跑一次对话"),
    detail: t("回到 {client} 里发一句话。成功后回到「调用」页，会看到这次调用的模型、耗时与 Token。", { client: clientNames[client] }),
    done: callCount > 0,
    status: callCount > 0 ? t("已有 {count} 条调用记录", { count: callCount }) : t("还没有调用记录"),
    path: "/calls",
    action: t("去调用页"),
  };
}

export function buildOnboarding(input: OnboardingInput): OnboardingView {
  const steps: OnboardingStep[] = [modelStep(input.modelCount)];
  if (input.client === "cursor") steps.push(cursorStep(input.cursor), verifyStep("cursor", input.callCounts.cursor ?? 0));
  else if (input.client === "devin") steps.push(devinStep(input.devin, input.hostConfigured, input.hostConfigRead), verifyStep("devin", input.callCounts.devin ?? 0));
  else if (input.client === "codex") steps.push(codexStep(input.codex), verifyStep("codex", input.callCounts.codex ?? 0));
  else if (input.client === "claude") steps.push(claudeStep(input.claude), verifyStep("claude", input.callCounts.claude ?? 0));
  return {
    steps,
    completed: steps.filter((step) => step.done).length,
    total: steps.length,
  };
}
