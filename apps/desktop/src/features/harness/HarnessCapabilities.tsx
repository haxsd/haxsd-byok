import { TitledCard } from "../../shared/ui/TitledCard";
import styles from "./HarnessCapabilities.module.scss";

type Client = "cursor" | "codex" | "devin" | "claude";

/**
 * One shared list for every takeover page: local features stay usable, while
 * vendor cloud/account features remain official. Keep this deliberately short;
 * the goal is to tell users where the boundary is, not to document every menu.
 */
export function HarnessCapabilities({ client }: { client: Client }) {
  let usable: string[] = [];
  let official: string[] = [];

  if (client === "cursor") {
    usable = [
      t("Agent、Chat、Plan 和本地子代理"),
      t("Rules、Skills、MCP、本地工具与 Checkpoint"),
      t("本地文件、终端、浏览器和提交信息"),
      t("模型库里的模型与连接测试"),
    ];
    official = [
      t("Cloud Agents、Projects、Bugbot 和 Automations"),
      t("Tab 补全的独立模型通道"),
      t("CLI、JetBrains/Zed/Neovim 集成"),
      t("Remote Control 的账号配对与云端中继"),
    ];
  } else if (client === "codex") {
    usable = [
      t("本地 Agent、TUI、桌面线程和 exec"),
      t("Plan、工具、审批、沙箱和多会话"),
      t("MCP、Skills、Plugins、Hooks 和本地审查"),
      t("图片输入与本地模型路由"),
    ];
    official = [
      t("ChatGPT Work、Codex Cloud 等云端任务"),
      t("Remote Control 的账号配对与中继"),
      t("官方托管 Web 搜索、图像生成和语音"),
      t("云端浏览器与厂商托管工具"),
    ];
  } else if (client === "devin") {
    usable = [
      t("Cascade、Devin Local 和本机工具"),
      t("Rules、Memories、MCP、终端与文件编辑"),
      t("模型选择、默认模型和本地 Checkpoint"),
      t("本地预览和终端执行"),
    ];
    official = [
      t("账号套餐限制的模型与额度"),
      t("Tab 补全、内联 Command 和 embedding"),
      t("Devin Cloud 会话与远程 indexing"),
      t("Slack、Linear、Jira、Teams、GitHub 集成"),
    ];
  } else {
    usable = [
      t("Claude Code CLI 会话、Agent 和工具"),
      t("子代理、后台会话、MCP、Skills 和 Hooks"),
      t("Checkpoint、Rewind 和自动压缩"),
      t("本地 IDE 集成与模型库路由"),
    ];
    official = [
      t("Cloud session、Teleport 和 Ultrareview"),
      t("云端定时任务与 Remote Control 中继"),
      t("依赖供应商的 WebSearch/WebFetch"),
      t("Claude Desktop handoff 和账号订阅"),
    ];
  }

  return (
    <TitledCard
      title={t("能力与限制")}
      description={t("本机可控制的模型请求走模型库；其余功能仍使用客户端官方服务。")}
    >
      <div className={styles.grid}>
        <section>
          <strong>{t("本机接管可用")}</strong>
          <ul>{usable.map((item) => <li key={item}>{item}</li>)}</ul>
        </section>
        <section>
          <strong>{t("需官方账号或云端")}</strong>
          <ul>{official.map((item) => <li key={item}>{item}</li>)}</ul>
        </section>
      </div>
    </TitledCard>
  );
}
