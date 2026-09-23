import type { ReactNode } from "react";
import styles from "./StatTile.module.scss";

export type StatTone = "none" | "ok" | "warn" | "bad" | "accent";

/**
 * One headline figure with its context.
 *
 * The dashboard used to present four figures in a single joined strip. It read well
 * as a row of numbers and badly as anything else: a sparkline, a ring or a second
 * line of explanation had nowhere to go, so the page's most important area was also
 * its least informative. A tile is the same figure with room around it.
 */
export function StatTile({ label, info, value, hint, tone = "none", visual, visualPosition = "bottom", footer, onClick, title }: {
  label: ReactNode;
  info?: ReactNode;
  value: ReactNode;
  hint?: ReactNode;
  tone?: StatTone;
  visual?: ReactNode;
  visualPosition?: "right" | "bottom";
  footer?: ReactNode;
  onClick?: () => void;
  title?: string;
}) {
  const content = <>
    <div className={styles.header}>
      <span className={styles.label}>{label}</span>
      {info}
    </div>
    <div className={styles.body} data-visual={visualPosition}>
      <div className={styles.figures}>
        <span className={styles.value} title={title} data-tone={tone}>{value}</span>
        {hint && <span className={styles.hint}>{hint}</span>}
      </div>
      {visual && visualPosition === "right" && <div className={styles.visualRight}>{visual}</div>}
    </div>
    {visual && visualPosition === "bottom" && <div className={styles.visualBottom}>{visual}</div>}
    {footer && <div className={styles.footer}>{footer}</div>}
  </>;

  if (!onClick) return <article className={styles.root}>{content}</article>;
  return <button type="button" className={styles.root} data-interactive onClick={onClick}>{content}</button>;
}
