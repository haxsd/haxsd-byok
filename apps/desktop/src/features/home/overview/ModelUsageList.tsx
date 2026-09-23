import { formatCompactInteger } from "../../../shared/utils/numberFormat";
import { Meter } from "../../../shared/ui/ProgressRing";
import { chartPalette } from "../charts/chartTheme";
import styles from "./ModelUsageList.module.scss";
import type { ModelUsageRow } from "./overviewStats";

/**
 * 模型用量排行。
 *
 * 模型库页回答「配了哪些模型」，这里回答「谁在干活」。两者用的是同一份数据，
 * 但问的问题不同，所以放在不同的页面上。
 */
export function ModelUsageList({ rows, limit = 5 }: { rows: ModelUsageRow[]; limit?: number }) {
  const palette = chartPalette();
  if (rows.length === 0) return <p className={styles.empty}>{t("这段时间内还没有模型调用。")}</p>;
  const visible = rows.slice(0, limit);
  const rest = rows.slice(limit);
  const restTokens = rest.reduce((total, row) => total + row.tokens, 0);
  const restCalls = rest.reduce((total, row) => total + row.calls, 0);

  return <ul className={styles.root}>
    {visible.map((row, index) => <li key={row.key} className={styles.row}>
      <span className={styles.name} title={row.key}>{row.label}</span>
      <span className={styles.share}>
        <Meter
          height={5}
          ariaLabel={t("{name} 占全部用量的 {percent}%", { name: row.label, percent: Math.round(row.share * 100) })}
          segments={[{ value: row.tokens, color: palette.series[index % palette.series.length], label: row.label }]}
        />
      </span>
      <span className={styles.value}>
        <strong>{formatCompactInteger(row.tokens)}</strong>
        <small>{t("{calls} 次", { calls: row.calls })}</small>
      </span>
    </li>)}
    {rest.length > 0 && <li className={styles.row}>
      <span className={styles.name}>{t("其他 {count} 个模型", { count: rest.length })}</span>
      <span className={styles.share} />
      <span className={styles.value}>
        <strong>{formatCompactInteger(restTokens)}</strong>
        <small>{t("{calls} 次", { calls: restCalls })}</small>
      </span>
    </li>}
    <li className={styles.note}>{t("按 Token 总量排序，含输入与输出。")}</li>
  </ul>;
}
