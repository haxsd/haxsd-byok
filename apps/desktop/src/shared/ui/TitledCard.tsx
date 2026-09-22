import { useState, type ReactNode } from "react";
import { Card } from "./Card";
import styles from "./TitledCard.module.scss";

/** Collapsed sections stay collapsed, so the choice is not re-made on every visit. */
const storagePrefix = "haxsd-byok:disclosure:";

function readStored(key: string): boolean | null {
  try {
    const value = localStorage.getItem(storagePrefix + key);
    return value === null ? null : value === "open";
  } catch {
    return null;
  }
}

function writeStored(key: string, open: boolean) {
  try {
    localStorage.setItem(storagePrefix + key, open ? "open" : "closed");
  } catch {
    // Persisting is best effort; the card still works for this session.
  }
}

/**
 * A titled card. With `collapsible` the body starts folded, which keeps
 * rarely-touched protocol and path settings out of the way without hiding them.
 * A `storageKey` remembers the open state across sessions.
 */
export function TitledCard({ title, action, collapsible = false, storageKey, children }: { title: ReactNode; action?: ReactNode; collapsible?: boolean; storageKey?: string; children: ReactNode }) {
  const [open, setOpen] = useState(() => {
    if (!collapsible) return true;
    // A remembered choice wins; otherwise a collapsible card starts folded.
    return storageKey ? readStored(storageKey) ?? false : false;
  });
  if (!collapsible) {
    return <Card as="section" className={styles.root}><header className={styles.header}><div className={styles.title}>{title}</div>{action}</header>{children}</Card>;
  }
  const toggle = () => {
    setOpen((current) => {
      const next = !current;
      if (storageKey) writeStored(storageKey, next);
      return next;
    });
  };
  return <Card as="section" className={styles.root}>
    <header className={styles.header}>
      <button type="button" className={styles.disclosure} aria-expanded={open} onClick={toggle}>
        <span className={styles.chevron} data-open={open} aria-hidden="true">›</span>
        <span className={styles.title}>{title}</span>
      </button>
      {action}
    </header>
    {open && children}
  </Card>;
}
