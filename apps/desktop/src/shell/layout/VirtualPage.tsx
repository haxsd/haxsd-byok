import { useCallback, type ReactNode } from "react";
import { VirtualList } from "../../shared/virtual/VirtualListEngine";
import styles from "./VirtualPage.module.scss";

export type VirtualPageSection = {
  key: string;
  estimatedHeight: number;
  content: ReactNode;
};

export function VirtualPage({ sections, className, contentClassName }: { sections: VirtualPageSection[]; className?: string; contentClassName?: string }) {
  const getKey = useCallback((section: VirtualPageSection) => section.key, []);
  const estimateSize = useCallback((section: VirtualPageSection) => section.estimatedHeight, []);
  const renderItem = useCallback((section: VirtualPageSection) => <section className={styles.section}>
    {section.content}
  </section>, []);

  return <VirtualList
    items={sections}
    getKey={getKey}
    estimateSize={estimateSize}
    renderItem={renderItem}
    overscan={2}
    /* The gap lives in the section's own padding so it comes from the theme's
       rhythm token, which the virtual list's pixel prop could not read. */
    itemGap={0}
    scrollbarSize={7}
    scrollbarInsetTop="var(--app-content-top)"
    className={[styles.root, "scroll-shadow-top", className].filter(Boolean).join(" ")}
    contentClassName={[styles.content, contentClassName].filter(Boolean).join(" ")}
  />;
}
