import { useEffect, useState } from "react";
import { useParams } from "react-router-dom";
import { api, type CallDetail } from "../../shared/api";
import { CallDetails } from "./CallDetails";
import { Button } from "../../shared/ui/Button";
import { EmptyState } from "../../shared/ui/EmptyState";
import { Icon } from "../../shared/ui/Icon";
import { StatusPill } from "../../shared/ui/StatusPill";
import { Card } from "../../shared/ui/Card";
import { callStatusLabel, callStatusTone } from "../../shared/utils/callStatus";
import { formatCompactInteger } from "../../shared/utils/numberFormat";
import { formatDuration } from "../../shared/utils/relativeTime";
import { alertCircleIcon, activityIcon, refreshIcon } from "../../shared/ui/icons";
import styles from "./CallDetailsPage.module.scss";
import { ScrollableContent } from "../../shared/virtual/ScrollableContent";

/**
 * 调用详情，在自己的窗口里打开。
 *
 * 这一页不带外壳，所以它必须自己交代清楚「这是哪一次调用」：标题带给出模型、状态、
 * 时间和用量，下面才是完整字段与原始报文。以前它只有一行 18px 的模型名，其余全靠读表。
 *
 * 读取失败必须自带「重试」：详情窗口没有导航，一次瞬时失败（服务正在重启）以前只会
 * 停在一句错误上，用户除了手动刷新窗口没有别的动作。
 */
export function CallDetailsPage() {
  const { callId = "" } = useParams();
  const [detail, setDetail] = useState<CallDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let cancelled = false;
    setDetail(null);
    setError(null);
    void api.call(callId).then((value) => {
      if (!cancelled) setDetail(value);
    }).catch((cause) => {
      if (!cancelled) setError(cause instanceof Error ? cause.message : String(cause));
    });
    return () => {
      cancelled = true;
    };
  }, [callId, attempt]);

  const call = detail?.call;
  const tokens = call ? call.total_tokens ?? (call.input_tokens ?? 0) + (call.output_tokens ?? 0) : 0;

  return <main className={styles.root}>
    <ScrollableContent className={styles.scroller} contentClassName={styles.content}>
      <header className={styles.header}>
        <div className={styles.titleRow}>
          {call && <StatusPill tone={callStatusTone(call.status)} title={call.status}>{callStatusLabel(call.status)}</StatusPill>}
          <h1>{call ? call.display_name || call.model_id : t("调用详情")}</h1>
          {call && <span className={styles.route} data-route={call.route}>{call.route === "cursor_official" ? t("Cursor 官方") : "BYOK"}</span>}
        </div>
        {call && <div className={styles.meta}>
          <span>{Number.isFinite(call.created_at_ms) ? new Date(call.created_at_ms).toLocaleString() : "-"}</span>
          <span>{formatDuration(call.duration_ms)}</span>
          <span>TTFB {formatDuration(call.ttfb_ms)}</span>
          <span>{t("{tokens} 个 Token", { tokens: formatCompactInteger(tokens) })}</span>
          <span className={styles.modelId}>{call.model_id}</span>
        </div>}
      </header>
      {error
        ? <Card className={styles.body}><EmptyState
          icon={alertCircleIcon}
          tone="bad"
          title={t("无法加载调用详情")}
          description={error}
          actions={<Button size="small" onClick={() => setAttempt((value) => value + 1)}>
            <Icon icon={refreshIcon} size="1em" />{t("重试")}
          </Button>}
        /></Card>
        : detail
          ? <Card className={styles.body}><div className={styles.bodyInner}><CallDetails detail={detail} /></div></Card>
          : <Card className={styles.body}><EmptyState icon={activityIcon} title={t("正在加载调用详情…")} /></Card>}
    </ScrollableContent>
  </main>;
}
