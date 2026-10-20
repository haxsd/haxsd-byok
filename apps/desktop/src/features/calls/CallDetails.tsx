import { useState } from "react";
import type { CallDetail } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { JsonEditor } from "../../shared/ui/JsonEditor";
import { Tabs, type TabItem } from "../../shared/ui/Tabs";
import { formatBytes } from "../settings/storageHint";
import { CallFacts } from "./CallFacts";
import { RESPONSE_PREVIEW_BYTES, responsePreview } from "./responsePreview";
import styles from "./CallDetails.module.scss";

/// 压缩触发方式的可读标签；未知值原样显示，后端加新触发方式时界面不会变成空白。
function compactionTriggerLabel(trigger: string) {
  switch (trigger) {
    case "preflight": return t("主动预检");
    case "context-overflow": return t("上游拒绝后补救");
    case "explicit": return t("手动压缩");
    default: return trigger;
  }
}

export function CallDetails({ detail }: { detail: CallDetail }) {
  const { call, request, cursor_trace: cursorTrace } = detail;
  // 响应也可能是空的：把所有取值都收在一个为空的默认值上，别让一个缺字段的
  // 响应把整个详情窗口打空（真实发生过）。
  const chunks = detail.response_chunks ?? [];
  // 大响应默认只渲染前缀：几 MB 的正文一次性交给 Monaco，打开详情要等两三秒。
  const [showAllResponse, setShowAllResponse] = useState(false);
  const response = responsePreview(chunks, { showAll: showAllResponse });

  const tabs: TabItem[] = [
    { value: "call", label: t("调用信息"), content: <section className={styles.section}>
      <CallFacts call={call} />
      {/* 压缩是"模型忘了前面的事"的唯一解释：在详情里当场说清楚压掉了多少、归档在哪。 */}
      {detail.compaction && <div className={styles.compaction}>
        <h4>{t("上下文压缩")}</h4>
        <div className={styles.meta}>
          {t("这次压缩把 {from} 条历史替换成 {to} 条（摘要 {chars} 字符）", {
            from: detail.compaction.compactable_messages,
            to: detail.compaction.replacement_messages,
            chars: detail.compaction.summary_chars,
          })}
        </div>
        <div className={styles.meta}>
          {t("触发方式：{trigger}", { trigger: compactionTriggerLabel(detail.compaction.trigger) })}
          {detail.compaction.retention === "none" ? ` · ${t("只保留摘要")}` : ""}
        </div>
        {detail.compaction.archive_path && <div className={styles.meta}>
          {t("压缩前的完整历史")}：<code>{detail.compaction.archive_path}</code>
        </div>}
      </div>}
    </section> },
    { value: "request", label: t("请求"), content: <section className={styles.section}>
      {request ? <>
        <div className={styles.meta}>{t("字节数")}：{request.byte_count}</div>
        <h4>{t("请求头")}</h4><JsonEditor ariaLabel={t("请求头")} value={JSON.stringify(request.headers)} readOnly />
        <h4>{t("请求体")}</h4><JsonEditor ariaLabel={t("请求体")} value={JSON.stringify(request.body)} readOnly detail />
      </> : <div className={styles.empty}>{t("未记录请求内容，请开启详细记录后重试。")}</div>}
    </section> },
    { value: "response", label: t("响应流"), content: <section className={styles.section}>
      {chunks.length > 0 ? <>
        <div className={styles.meta}>{t("分块数")}：{chunks.length} · {t("字节数")}：{response.totalBytes}</div>
        {response.truncated && <div className={styles.meta}>
          <span>{t("仅显示前 {shown}（共 {total}）", { shown: formatBytes(response.shownBytes), total: formatBytes(response.totalBytes) })}</span>
          <Button size="small" onClick={() => setShowAllResponse(true)}>{t("显示全部")}</Button>
        </div>}
        <JsonEditor ariaLabel={t("响应流")} value={response.text} readOnly detail />
        {response.truncated && <small>{t("默认只渲染前 {limit}；点「显示全部」会一次性加载整段响应。", { limit: formatBytes(RESPONSE_PREVIEW_BYTES) })}</small>}
      </> : <div className={styles.empty}>{t("未记录响应内容，请开启详细记录后重试。")}</div>}
    </section> },
  ];

  if (cursorTrace) tabs.push({ value: "cursor-trace", label: t("Cursor 追踪"), content: <section className={styles.section}>
      <div className={styles.meta}>
        Request ID：{cursorTrace.trace.request_id} · {t("工件数")}：{cursorTrace.artifacts.length}
      </div>
      <JsonEditor
        ariaLabel={t("Cursor 追踪")}
        value={JSON.stringify({ trace: cursorTrace.trace, artifacts: cursorTrace.artifacts })}
        readOnly
        detail
      />
    </section> });

  return <div className={styles.root}><Tabs items={tabs} /></div>;
}
