import type { ReactNode } from "react";
import { Icon } from "./Icon";
import type { IconifyIcon } from "@iconify/react";
import styles from "./StatusPill.module.scss";

export type StatusTone = "ok" | "warn" | "bad" | "info" | "idle";

/**
 * One vocabulary for state across every page.
 *
 * Before this, the same three states were drawn as coloured text in the call table,
 * as a filled badge in the sidebar, as a lamp with a halo on the Devin page and as a
 * bordered box on the model cards. Each looked deliberate on its own; together they
 * made the product look assembled from four applications.
 */
export function StatusPill({ tone = "idle", icon, children, title }: {
  tone?: StatusTone;
  icon?: IconifyIcon;
  children: ReactNode;
  title?: string;
}) {
  return <span className={styles.root} data-tone={tone} title={title}>
    {icon ? <Icon icon={icon} size="1em" /> : <span className={styles.dot} aria-hidden="true" />}
    <span className={styles.label}>{children}</span>
  </span>;
}
