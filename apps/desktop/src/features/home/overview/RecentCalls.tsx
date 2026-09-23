import type { LlmCall } from "../../../shared/api";
import { useI18n } from "../../../i18n/store";
import { callStatusTone } from "../../../shared/utils/callStatus";
import { formatCompactInteger } from "../../../shared/utils/numberFormat";
import { formatDuration, formatRelativeTime } from "../../../shared/utils/relativeTime";
import { Icon } from "../../../shared/ui/Icon";
import { TooltipTrigger } from "../../../shared/ui/TooltipTrigger";
import { eyeIcon } from "../../../shared/ui/icons";
import styles from "./RecentCalls.module.scss";

/**
 * 最近几条调用。
 *
 * 图表回答「这段时间用了多少」，回答不了「刚刚发生了什么」——而后者才是打开这个
 * 页面时最常想知道的事。所以把调用记录按时间倒序取前几条，和图表并排放。
 */
export function RecentCalls({ calls, limit = 6, onOpen }: {
  calls: LlmCall[];
  limit?: number;
  onOpen: (call: LlmCall) => void;
}) {
  const { locale } = useI18n();
  const rows = calls.slice(0, limit);
  if (rows.length === 0) return <p className={styles.empty}>{t("还没有调用记录。接好模型后，这里会实时出现每一次调用。")}</p>;
  return <ul className={styles.root}>
    {rows.map((call) => {
      const tokens = call.total_tokens ?? (call.input_tokens ?? 0) + (call.output_tokens ?? 0);
      return <li key={call.call_id}>
        <button type="button" className={styles.row} onClick={() => onOpen(call)}>
          <span className={styles.tone} data-tone={callStatusTone(call.status)} aria-hidden="true" />
          <span className={styles.identity}>
            <span className={styles.name}>{call.display_name || call.model_id}</span>
            <span className={styles.meta}>
              {formatRelativeTime(call.created_at_ms, locale)}
              {call.duration_ms !== null && ` · ${formatDuration(call.duration_ms)}`}
            </span>
          </span>
          <span className={styles.tokens}>{tokens > 0 ? formatCompactInteger(tokens) : "-"}</span>
          <TooltipTrigger label={t("查看详情")}>
            <span className={styles.open} aria-hidden="true"><Icon icon={eyeIcon} size="1em" /></span>
          </TooltipTrigger>
        </button>
      </li>;
    })}
  </ul>;
}
