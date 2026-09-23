import type { IconifyIcon } from "@iconify/react";
import type { ReactNode } from "react";
import { Icon } from "./Icon";
import styles from "./EmptyState.module.scss";

export type EmptyStep = {
  title: string;
  detail?: string;
};

/**
 * The one shape for "there is nothing here yet" and "this needs setting up".
 *
 * These states used to be a dashed rectangle with two centred lines and a button,
 * which is how a first-run user met five of the seven pages. The dashed border is
 * gone: an empty page is a page with an instruction, not a hole in the layout.
 */
export function EmptyState({ icon, tone = "info", title, description, steps, actions, compact = false }: {
  icon?: IconifyIcon;
  tone?: "info" | "warn" | "bad";
  title: ReactNode;
  description?: ReactNode;
  steps?: EmptyStep[];
  actions?: ReactNode;
  compact?: boolean;
}) {
  return <div className={styles.root} data-tone={tone} data-compact={compact || undefined}>
    {icon && <div className={styles.badge} aria-hidden="true"><Icon icon={icon} size="1.4em" /></div>}
    <div className={styles.body}>
      <strong className={styles.title}>{title}</strong>
      {description && <p className={styles.description}>{description}</p>}
      {steps && steps.length > 0 && <ol className={styles.steps}>
        {steps.map((step, index) => <li key={step.title}>
          <span className={styles.stepIndex} aria-hidden="true">{index + 1}</span>
          <span className={styles.stepText}>
            <span className={styles.stepTitle}>{step.title}</span>
            {step.detail && <span className={styles.stepDetail}>{step.detail}</span>}
          </span>
        </li>)}
      </ol>}
    </div>
    {actions && <div className={styles.actions}>{actions}</div>}
  </div>;
}
