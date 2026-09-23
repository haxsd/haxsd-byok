import type { ReactNode } from "react";
import styles from "./PageTitle.module.scss";

/**
 * The page's name in the title band, with the one fact that explains the page next
 * to it.
 *
 * The band used to hold the page name and nothing else, so every page opened with a
 * bare word — "Cursor", "调用" — while the state the reader came for (is it standing
 * up? how many rows? which range?) was somewhere down the page or in the toolbar.
 */
export function PageTitle({ title, status, meta }: {
  title: ReactNode;
  status?: ReactNode;
  meta?: ReactNode;
}) {
  return <div className={styles.root}>
    <h1 className={styles.title}>{title}</h1>
    {status}
    {meta && <span className={styles.meta}>{meta}</span>}
  </div>;
}
