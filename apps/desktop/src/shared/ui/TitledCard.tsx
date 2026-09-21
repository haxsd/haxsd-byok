import { useState, type ReactNode } from "react";
import { Card } from "./Card";
import styles from "./TitledCard.module.scss";

/**
 * A titled card. With `collapsible` the body starts folded, which keeps
 * rarely-touched protocol and path settings out of the way without hiding them.
 */
export function TitledCard({ title, action, collapsible = false, children }: { title: ReactNode; action?: ReactNode; collapsible?: boolean; children: ReactNode }) {
  const [open, setOpen] = useState(!collapsible);
  if (!collapsible) {
    return <Card as="section" className={styles.root}><header className={styles.header}><div className={styles.title}>{title}</div>{action}</header>{children}</Card>;
  }
  return <Card as="section" className={styles.root}>
    <header className={styles.header}>
      <button type="button" className={styles.disclosure} aria-expanded={open} onClick={() => setOpen((current) => !current)}>
        <span className={styles.chevron} data-open={open} aria-hidden="true">›</span>
        <span className={styles.title}>{title}</span>
      </button>
      {action}
    </header>
    {open && children}
  </Card>;
}
