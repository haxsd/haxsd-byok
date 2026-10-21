import type { CodexStatus } from "../../shared/api";
import { TitledCard } from "../../shared/ui/TitledCard";
import { codexLoginState } from "./codexConnectionView";
import styles from "./CodexSettingsPage.module.scss";

/**
 * 「登录与接管」说明卡。
 *
 * 用户最容易误解的两件事都写在这里：接管不会退出/替换 ChatGPT 登录；以及正确的顺序是
 * 先登录、再接管（接管后客户端可能不再提供登录入口）。界面语言、云端会话等由客户端
 * 自己的账号和运行时/远端配置决定，接管不参与——不要把这些差异归因成接管 bug。
 */
export function CodexLoginCard({ status }: { status: CodexStatus | null }) {
  const login = codexLoginState(status);
  return <TitledCard title={t("登录与接管")}>
    <div className={styles.notes}>
      <p>{t("接管只把 Codex 的模型请求改到本机网关（使用本机令牌），不会退出或替换你的 ChatGPT 登录，也不会修改 auth.json；登录态仍用于客户端自己的账号功能。")}</p>
      <p>{t("推荐顺序：先在 Codex 客户端登录 ChatGPT，再应用接管。未登录时接管会把客户端保持在 Codex 本地模式（「输出详细程度」保持默认），否则新版客户端会用需要登录的 ChatGPT Work 模式拒绝本地任务；登录后会还回你原来的详细模式。")}</p>
      {login === "signed-in" && <p>{t("已检测到 ChatGPT 登录，可以应用接管。")}</p>}
      {login === "signed-out" && <p className={styles.warning} role="status">{t("未检测到 ChatGPT 登录：建议先在 Codex 客户端登录，再应用接管。")}</p>}
    </div>
  </TitledCard>;
}
