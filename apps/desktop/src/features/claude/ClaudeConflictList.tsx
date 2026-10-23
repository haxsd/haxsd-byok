import type { ClaudeTakeoverConflict } from "../../shared/api";
import { claudeConflictGroups } from "./claudeConnectionView";
import styles from "./ClaudeSettingsPage.module.scss";

/**
 * 服务端的冲突与提示。
 *
 * `blocking` 决定它在写入路径上的角色：阻断项用红色区块，提示项用中性区块。文案原样
 * 来自服务端（`message`），`code` / `key` 只用于分组与列表定位，不在界面重写。
 */
export function ClaudeConflictList({ conflicts }: { conflicts: ClaudeTakeoverConflict[] }) {
  const { blocking, hints } = claudeConflictGroups(conflicts);
  if (!conflicts.length) return null;

  return <>
    {blocking.length > 0 && <section className={styles.conflict} aria-label={t("冲突与阻断")}>
      <strong>{t("冲突与阻断")}</strong>
      <ul>
        {blocking.map((conflict, index) => <li
          key={`${conflict.code}-${conflict.key ?? index}`}
          data-code={conflict.code}
        >
          {conflict.message}
        </li>)}
      </ul>
      <small>{t("以上条目原样来自服务端；处理完再应用接管。")}</small>
    </section>}

    {hints.length > 0 && <section className={styles.hint} aria-label={t("提示")}>
      <strong>{t("提示")}</strong>
      <ul>
        {hints.map((conflict, index) => <li
          key={`${conflict.code}-${conflict.key ?? index}`}
          data-code={conflict.code}
        >
          {conflict.message}
        </li>)}
      </ul>
      <small>{t("这些条目不阻止接管：它们只影响从本应用之外启动的客户端。")}</small>
    </section>}
  </>;
}
