import type { IconifyIcon } from "@iconify/react";
import cursorIconUrl from "../../shared/assets/icons/cursor.svg";
import { navCodexIcon, navDevinIcon } from "../../shared/ui/navIcons";
import { terminalIcon } from "../../shared/ui/icons";
import type { ClaudeStatus, ClaudeTakeover, CodexStatus, CursorHarnessStatus, DevinStatus } from "../../shared/api";
import type { EntryStatus, StatusTone } from "../../shell/connectionStatus";
import { cursorEntryStatus, devinEntryStatus } from "../../shell/connectionStatus";
import { codexConnectionView } from "../codex/codexConnectionView";
import { claudeConnectionView } from "../claude/claudeConnectionView";

/**
 * 客户端接入模块的清单。
 *
 * 导航栏只留一个「接入」入口，具体客户端在接入页里切换：接一个新客户端时改这里
 * 一处即可，顶栏不会跟着变长。每个客户端自己的页面、路由与状态推导都留在原处，
 * 模块之间仍然互不影响。
 */
export interface HarnessClientEntry {
  /** 该客户端是否正在通过本机网关工作；聚合文案只用它计数。 */
  active: boolean;
  status: EntryStatus<string> | null;
}

export interface HarnessClient {
  id: string;
  path: string;
  label: string;
  icon: IconifyIcon | string;
  entry(state: HarnessState): HarnessClientEntry;
}

export interface HarnessState {
  cursorHarness: CursorHarnessStatus | null;
  devinStatus: DevinStatus | null;
  codexStatus: CodexStatus | null;
  claudeStatus: ClaudeStatus | null;
  claudeTakeover: ClaudeTakeover | null;
}

export const harnessClients: HarnessClient[] = [
  {
    id: "cursor",
    path: "/harness/cursor",
    label: "Cursor",
    icon: cursorIconUrl,
    entry: ({ cursorHarness }) => {
      const status = cursorEntryStatus(cursorHarness);
      return { active: status?.state === "taken-over", status };
    },
  },
  {
    id: "devin",
    path: "/harness/devin",
    label: "Devin",
    icon: navDevinIcon,
    entry: ({ devinStatus }) => {
      const status = devinEntryStatus(devinStatus);
      return { active: status?.state === "running", status };
    },
  },
  {
    id: "codex",
    path: "/harness/codex",
    label: "Codex",
    icon: navCodexIcon,
    entry: ({ codexStatus }) => {
      const status = codexConnectionView(codexStatus);
      return { active: status.state === "taken-over", status };
    },
  },
  {
    id: "claude",
    path: "/harness/claude",
    label: "Claude",
    // 通用终端图标：这里不打包 Anthropic / Claude 的商标 Logo。
    icon: terminalIcon,
    entry: ({ claudeStatus, claudeTakeover }) => {
      const status = claudeConnectionView(claudeStatus, claudeTakeover);
      return { active: status.state === "taken-over", status };
    },
  },
];

/** 当前路径属于哪个客户端；不属于任何客户端时返回 null。 */
export function harnessClientAt(pathname: string): HarnessClient | null {
  return harnessClients.find(
    (client) => pathname === client.path || pathname.startsWith(`${client.path}/`),
  ) ?? null;
}

/**
 * 顶部「接入」入口的状态：文案只用计数，单个客户端的细节放在页面内的切换项里。
 *
 * 部分接入是正常用法（很多人只用一两个客户端），不能因为没接满就整项变黄；
 * 只有硬冲突仍用红色提示。
 */
export function harnessEntryStatus(state: HarnessState): EntryStatus<string> {
  const entries = harnessClients.map((client) => client.entry(state));
  const active = entries.filter((entry) => entry.active).length;
  const total = entries.length;
  const tone: StatusTone = entries.some((entry) => entry.status?.tone === "bad")
    ? "bad"
    : active === total ? "ok" : "idle";
  return {
    state: active === 0 ? "none-active" : `${active}-active`,
    tone,
    label: active === 0 ? t("未接入") : t("{active}/{total} 已接入", { active, total }),
  };
}
