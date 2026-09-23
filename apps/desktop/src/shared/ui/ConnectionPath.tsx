import { Fragment } from "react";
import styles from "./ConnectionPath.module.scss";

export type PathState = "up" | "down" | "unknown";

export type PathStage = {
  key: string;
  /** Node name, e.g. "Cursor" or "本机网关". */
  label: string;
  /** The fact that makes this node up or down, e.g. "已指向本机网关". */
  detail: string;
  state: PathState;
};

/**
 * The request path: the client, this machine's gateway, and the model library.
 *
 * It exists to answer the one question a reader arrives with — is a request from this
 * client reaching a model right now — before anything else on the page. Both harness
 * pages draw the same wire, because they answer the same question about a different
 * client; the old layout spread that answer over four status values and a checklist,
 * so the reader had to assemble it. Here the state lives on the wire: a node is a
 * point on it, lit when traffic can pass and hollow when it cannot, so a broken hop
 * is visible without reading a word.
 */
export function ConnectionPath({ stages }: { stages: PathStage[] }) {
  return <div className={styles.root}>
    {stages.map((stage, index) => {
      const previous = stages[index - 1];
      // A hop can only carry traffic if both of its ends can, which is what makes a
      // single dark segment readable as "the break is here".
      const lit = previous !== undefined && previous.state === "up" && stage.state === "up";
      return <Fragment key={stage.key}>
        {previous !== undefined && <div className={styles.link} data-lit={lit || undefined} aria-hidden="true" />}
        <div className={styles.node} data-state={stage.state}>
          <span className={styles.dot} aria-hidden="true" />
          <span className={styles.name}>{stage.label}</span>
          <span className={styles.detail}>{stage.detail}</span>
        </div>
      </Fragment>;
    })}
  </div>;
}
