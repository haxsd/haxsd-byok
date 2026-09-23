import type { ReactNode } from "react";
import styles from "./SectionHeading.module.scss";

/**
 * A heading for a group of cards on a page that needs more than one group.
 *
 * Pages used to open with a stack of equally-weighted cards and no headings at all,
 * so the only way to find the section you wanted was to read every card title. The
 * eyebrow line states what the group answers; the title states what it is.
 */
export function SectionHeading({ eyebrow, title, description, actions, id }: {
  eyebrow?: ReactNode;
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  id?: string;
}) {
  return <div className={styles.root}>
    <div className={styles.text}>
      {eyebrow && <span className={styles.eyebrow}>{eyebrow}</span>}
      <h2 className={styles.title} id={id}>{title}</h2>
      {description && <p className={styles.description}>{description}</p>}
    </div>
    {actions && <div className={styles.actions}>{actions}</div>}
  </div>;
}
