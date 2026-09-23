import type { ReactNode } from "react";
import { ConnectionPath, type PathStage } from "./ConnectionPath";
import styles from "./StatusHero.module.scss";

export type StatusStep = {
  key: string;
  label: string;
  hint: string;
};

/**
 * 一个模块的接入状态：一句话结论、请求路径、以及还差哪几步。
 *
 * Cursor 页和 Devin 页问的是同一个问题——「现在有请求真的走到模型了吗」——
 * 所以答案的形状也必须一样。这段布局原本在 Devin 页里写了一份、Cursor 页里没有，
 * 于是两个并列模块一个像仪表盘、一个像设置表单。
 *
 * 只显示**还没完成**的步骤：全都通了以后还挂着四个勾，等于每次访问都要重新扫一遍。
 */
export function StatusHero({ connected, title, description, stages, steps, aside, footnotes }: {
  connected: boolean;
  title: ReactNode;
  description: ReactNode;
  stages: PathStage[];
  steps: StatusStep[];
  /** 右侧的补充信息，例如本地 CA 状态或接管开关。 */
  aside?: ReactNode;
  footnotes?: ReactNode;
}) {
  return <div className={styles.root}>
    <div className={styles.verdict} data-tone={connected ? "ok" : "warn"}>
      <span className={styles.lamp} aria-hidden="true" />
      <div className={styles.text}>
        <strong>{title}</strong>
        <small>{description}</small>
      </div>
      {aside && <div className={styles.aside}>{aside}</div>}
    </div>
    <ConnectionPath stages={stages} />
    {steps.length > 0 && <div className={styles.todo}>
      {steps.map((step, index) => <div key={step.key} className={styles.todoItem}>
        <span className={styles.todoIndex}>{index + 1}</span>
        <span className={styles.todoText}><strong>{step.label}</strong><small>{step.hint}</small></span>
      </div>)}
    </div>}
    {footnotes && <div className={styles.footnotes}>{footnotes}</div>}
  </div>;
}
