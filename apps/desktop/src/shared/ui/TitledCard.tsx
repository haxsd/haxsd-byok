import { useEffect, useState, type ReactNode } from "react";
import type { IconifyIcon } from "@iconify/react";
import { saveUiPreferences, useUiPreferences } from "../store/uiPreferences";
import { Card } from "./Card";
import { Icon } from "./Icon";
import { chevronRightIcon } from "./icons";
import styles from "./TitledCard.module.scss";

/**
 * A titled card. With `collapsible` the body starts folded, which keeps
 * rarely-touched protocol and path settings out of the way without hiding them.
 * A `storageKey` remembers the open state across sessions.
 *
 * 记住的地方是**服务端**（`uiPreferences.ts`）：以前写 localStorage，而那份值绑在管理端口
 * origin 上，端口一变折叠状态就全丢。启动时偏好异步读回来，所以先按折叠渲染、读到值再跟上。
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
  const remembered = useUiPreferences().disclosure?.[storageKey ?? ""];
  const [open, setOpen] = useState(() => {
    if (!collapsible) return true;
    // A remembered choice wins; otherwise a collapsible card starts folded.
    return typeof remembered === "boolean" ? remembered : false;
  });

  // 启动时偏好是异步到的：值到了要跟上，否则用户上次展开的卡片这次会一直折着。
  useEffect(() => {
    if (!collapsible || !storageKey || typeof remembered !== "boolean") return;
    setOpen(remembered);
  }, [collapsible, storageKey, remembered]);

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
      // 写服务端；界面不等它（折叠要立刻响应）。失败只影响"下次启动还记不记得"。
      if (storageKey) void saveUiPreferences({ disclosure: { [storageKey]: next } });
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
