import { useMemo, useState } from "react";
import type { LlmCall } from "../../shared/api";
import { useI18n } from "../../i18n/store";
import { ScrollableContent } from "../../shared/virtual/ScrollableContent";
import { Pagination } from "../../shared/ui/Pagination";
import { Meter } from "../../shared/ui/ProgressRing";
import { StatusPill } from "../../shared/ui/StatusPill";
import { Icon } from "../../shared/ui/Icon";
import { TooltipTrigger } from "../../shared/ui/TooltipTrigger";
import { EmptyState } from "../../shared/ui/EmptyState";
import { CallFacts } from "./CallFacts";
import { chartPalette } from "../home/charts/chartTheme";
import { callStatusLabel, callStatusTone } from "../../shared/utils/callStatus";
import { formatCompactInteger } from "../../shared/utils/numberFormat";
import { formatDuration, formatRelativeTime } from "../../shared/utils/relativeTime";
import { chevronRightIcon, eyeIcon, activityIcon } from "../../shared/ui/icons";
import styles from "./CallTable.module.scss";

type SortKey = "created_at" | "duration" | "tokens";

export function CallTable({ calls, onDetails }: { calls: LlmCall[]; onDetails: (call: LlmCall) => void }) {
  const { locale } = useI18n();
  const palette = chartPalette();
  const [expanded, setExpanded] = useState<string | null>(null);
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(50);
  const [sort, setSort] = useState<{ key: SortKey; descending: boolean }>({ key: "created_at", descending: true });

  const sorted = useMemo(() => {
    const value = (call: LlmCall) => sort.key === "created_at" ? call.created_at_ms
      : sort.key === "duration" ? call.duration_ms ?? -1
      : call.total_tokens ?? (call.input_tokens ?? 0) + (call.output_tokens ?? 0);
    return [...calls].sort((left, right) => (value(left) - value(right)) * (sort.descending ? -1 : 1));
  }, [calls, sort]);
  const pageCount = Math.max(1, Math.ceil(sorted.length / pageSize));
  const currentPage = Math.min(page, pageCount);
  const rows = sorted.slice((currentPage - 1) * pageSize, currentPage * pageSize);

  const toggleSort = (key: SortKey) => setSort((current) => current.key === key
    ? { key, descending: !current.descending }
    : { key, descending: true });

  if (calls.length === 0) return <EmptyState
    icon={activityIcon}
    title={t("没有符合条件的调用")}
    description={t("清掉筛选条件再试，或先在 Cursor / Devin 里发起一次对话。")}
  />;

  return <div className={styles.root}>
    <ScrollableContent
      className={styles.viewport}
      contentClassName={styles.tableContent}
      scrollbarInsetTop="var(--app-content-top)"
    >
      <div className={styles.table} role="table" aria-label={t("调用记录")}>
        <div className={styles.head} role="row">
          <span className={styles.expanderCell} aria-hidden="true" />
          <span role="columnheader">{t("状态")}</span>
          <span role="columnheader">{t("模型")}</span>
          <button type="button" role="columnheader" className={styles.sortable} aria-sort={sort.key === "created_at" ? (sort.descending ? "descending" : "ascending") : "none"} onClick={() => toggleSort("created_at")}>
            {t("时间")}<span className={styles.sortMark} data-active={sort.key === "created_at" || undefined}>{sort.key === "created_at" && !sort.descending ? "↑" : "↓"}</span>
          </button>
          <span role="columnheader">{t("路由")}</span>
          <button type="button" role="columnheader" className={styles.sortable} aria-sort={sort.key === "duration" ? (sort.descending ? "descending" : "ascending") : "none"} onClick={() => toggleSort("duration")}>
            {t("耗时")}<span className={styles.sortMark} data-active={sort.key === "duration" || undefined}>{sort.key === "duration" && !sort.descending ? "↑" : "↓"}</span>
          </button>
          <span role="columnheader">TTFB</span>
          <button type="button" role="columnheader" className={styles.sortable} aria-sort={sort.key === "tokens" ? (sort.descending ? "descending" : "ascending") : "none"} onClick={() => toggleSort("tokens")}>
            Token<span className={styles.sortMark} data-active={sort.key === "tokens" || undefined}>{sort.key === "tokens" && !sort.descending ? "↑" : "↓"}</span>
          </button>
          <span className={styles.actionsCell} role="columnheader">{t("操作")}</span>
        </div>
        {rows.map((call) => {
          const total = call.total_tokens ?? (call.input_tokens ?? 0) + (call.output_tokens ?? 0);
          const isOpen = expanded === call.call_id;
          return <div key={call.call_id} className={styles.rowGroup}>
            <div className={styles.row} role="row" data-open={isOpen || undefined}>
              <button
                type="button"
                className={styles.expanderCell}
                aria-expanded={isOpen}
                aria-label={isOpen ? t("收起详情") : t("展开详情")}
                onClick={() => setExpanded(isOpen ? null : call.call_id)}
              >
                <Icon icon={chevronRightIcon} size="1em" className={styles.chevron} data-open={isOpen || undefined} />
              </button>
              <span role="cell"><StatusPill tone={callStatusTone(call.status)} title={call.status}>{callStatusLabel(call.status)}</StatusPill></span>
              <span className={styles.model} role="cell">
                <span className={styles.modelName} title={call.display_name}>{call.display_name || call.model_id}</span>
                <span className={styles.modelId} title={call.model_id}>{call.model_id}</span>
              </span>
              <span className={styles.time} role="cell" title={new Date(call.created_at_ms).toLocaleString()}>
                {formatRelativeTime(call.created_at_ms, locale)}
              </span>
              <span role="cell">
                <span className={styles.route} data-route={call.route}>{call.route === "cursor_official" ? t("官方") : "BYOK"}</span>
              </span>
              <span className={styles.number} role="cell">{formatDuration(call.duration_ms)}</span>
              <span className={styles.number} role="cell">{formatDuration(call.ttfb_ms)}</span>
              <span className={styles.tokens} role="cell">
                <span className={styles.tokenValue}>{total > 0 ? formatCompactInteger(total) : "-"}</span>
                <span className={styles.tokenMeter}>
                  <Meter
                    height={4}
                    ariaLabel={t("{tokens} Token 的构成", { tokens: formatCompactInteger(total) })}
                    segments={[
                      { value: call.input_tokens ?? 0, color: palette.input, label: t("输入（非缓存）") },
                      { value: call.cache_read_tokens ?? 0, color: palette.cacheRead, label: t("缓存输入") },
                      { value: call.cache_write_tokens ?? 0, color: palette.cacheWrite, label: t("缓存写入") },
                      { value: call.output_tokens ?? 0, color: palette.output, label: t("模型输出") },
                    ]}
                  />
                </span>
              </span>
              <span className={styles.actionsCell} role="cell">
                <TooltipTrigger label={t("查看详情")}>
                  <button className={styles.rowAction} aria-label={t("查看详情")} onClick={() => onDetails(call)}>
                    <Icon icon={eyeIcon} size="1.05em" />
                  </button>
                </TooltipTrigger>
              </span>
            </div>
            {isOpen && <div className={styles.expansion}><CallFacts call={call} /></div>}
          </div>;
        })}
      </div>
    </ScrollableContent>
    <Pagination
      page={currentPage}
      pageCount={pageCount}
      pageSize={pageSize}
      total={sorted.length}
      onPageChange={setPage}
      onPageSizeChange={(next) => { setPageSize(next); setPage(1); }}
    />
  </div>;
}
