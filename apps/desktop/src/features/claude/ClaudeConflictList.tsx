import type { ClaudeTakeoverConflict } from "../../shared/api";
import { claudeConflictGroups } from "./claudeConnectionView";
import styles from "./ClaudeSettingsPage.module.scss";

/**
 * 服务端的冲突与提示，以及"配置已过期"的重新应用入口。
 *
 * `blocking` 决定它在写入路径上的角色：阻断项用红色区块，提示项用中性区块。文案原样
 * 来自服务端（`message`），`code` / `key` 只用于分组与列表定位，不在界面重写。
 *
 * `stale` 是客户端状态而不是服务端冲突条目：收据和文件还对得上，只是网关地址、令牌或
 * 模型映射变了。它排在冲突列表上方，直接指出下一步动作（重新应用），不再让用户从一条
 * `takeover_stale` 提示里自己推断。
 */
export function ClaudeConflictList({ conflicts, stale = false }: { conflicts: ClaudeTakeoverConflict[]; stale?: boolean }) {
  const { blocking, hints } = claudeConflictGroups(conflicts);
  if (!conflicts.length && !stale) return null;

  return <>
    {stale && <section className={styles.stale} aria-label={t("配置已过期")}>
      <strong>{t("配置已过期")}</strong>
      <small>{t("网关设置已变化，客户端仍指向旧地址/令牌，请重新应用接管。")}</small>
    </section>}

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
