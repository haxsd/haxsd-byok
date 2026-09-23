import type { ReactNode } from "react";
import type { VirtualPageSection } from "./VirtualPage";
import { VirtualPage } from "./VirtualPage";
import styles from "./PageContent.module.scss";

export function PageContent({ title, sections, contentClassName, fixed = false }: { title?: ReactNode; sections: VirtualPageSection[]; contentClassName?: string; fixed?: boolean }) {
  return <div className={styles.root}>
    <div className={styles.topScrim} aria-hidden="true" />
    {/* 标题带固定在页面上，不跟着内容滚。
        以前虚拟页面把标题放在第一段内容里，而虚拟行带 transform，会自成一个层叠
        上下文：标题的 z-index 只在那一行内部有效，于是任何覆盖在顶部的图层（顶部
        底色、导航条）都会把标题一起盖掉，滚动时标题还会跟着内容滑走。 */}
    {title != null && <div className={styles.title}>{title}</div>}
    {fixed
      ? <div className={[styles.fixedContent, contentClassName].filter(Boolean).join(" ")}>{sections.map((section) => <section className={styles.fixedSection} key={section.key}>{section.content}</section>)}</div>
      : <VirtualPage sections={sections} contentClassName={contentClassName} />}
  </div>;
}
