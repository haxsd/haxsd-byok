import { useState, type ReactNode } from "react";
import type { IconifyIcon } from "@iconify/react";
import { Card } from "./Card";
import { Icon } from "./Icon";
import { chevronRightIcon } from "./icons";
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
 *
 * The header now carries a short line under the title and an optional state slot:
 * card titles were the only thing on a page, so a reader had to open every card to
 * find out which one was worth opening.
 */
export function TitledCard({ title, description, icon, badge, action, collapsible = false, storageKey, children }: {
  title: ReactNode;
  description?: ReactNode;
  icon?: IconifyIcon;
  badge?: ReactNode;
  action?: ReactNode;
  collapsible?: boolean;
  storageKey?: string;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(() => {
    if (!collapsible) return true;
    // A remembered choice wins; otherwise a collapsible card starts folded.
    return storageKey ? readStored(storageKey) ?? false : false;
  });

  const heading = <>
    {icon && <span className={styles.icon}><Icon icon={icon} size="1.15em" /></span>}
    <span className={styles.heading}>
      <span className={styles.title}>{title}</span>
      {description && <span className={styles.description}>{description}</span>}
    </span>
    {badge}
  </>;

  if (!collapsible) {
    return <Card as="section" className={styles.root}>
      <header className={styles.header}>
        <div className={styles.headingRow}>{heading}</div>
        {action}
      </header>
      {children}
    </Card>;
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
        <span className={styles.chevron} data-open={open} aria-hidden="true"><Icon icon={chevronRightIcon} size="1em" /></span>
        {heading}
      </button>
      {action}
    </header>
    {open && children}
  </Card>;
}
