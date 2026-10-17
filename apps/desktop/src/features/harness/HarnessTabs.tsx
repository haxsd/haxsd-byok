import { NavLink } from "react-router-dom";
import { Icon } from "../../shared/ui/Icon";
import { TooltipTrigger } from "../../shared/ui/TooltipTrigger";
import { harnessClients, type HarnessState } from "./harnessClients";
import styles from "./HarnessTabs.module.scss";

/**
 * 接入模块的客户端切换，放在导航条下面那条切换带上（见 AppLayout 的 .harnessBar）。
 *
 * 那一带是它自己的布局区域：接入页的标题带与页面内容整体让开一条带高，因此这里
 * 不覆盖页面标题，也不与页面里那句说明争同一行。每个切换项自己带状态，原来挂在
 * 导航项上的那行状态没有丢。
 */
export function HarnessTabs({ state }: { state: HarnessState }) {
  return <nav className={styles.tabs} aria-label={t("接入模块")}>
    {harnessClients.map((client) => {
      const status = client.entry(state).status;
      return <TooltipTrigger key={client.id} label={status ? `${client.label} · ${status.label}` : client.label}>
        <NavLink to={client.path} className={styles.tab}>
          {typeof client.icon === "string"
            ? <Icon src={client.icon} size="1.05em" />
            : <Icon icon={client.icon} size="1.05em" />}
          <span className={styles.label}>{client.label}</span>
          {status && <span className={styles.status} data-tone={status.tone}>{status.label}</span>}
        </NavLink>
      </TooltipTrigger>;
    })}
  </nav>;
}
