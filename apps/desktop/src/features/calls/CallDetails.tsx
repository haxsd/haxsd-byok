import { useMemo, useState } from "react";
import type { CallDetail, LlmCall } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { JsonEditor } from "../../shared/ui/JsonEditor";
import { Tabs, type TabItem } from "../../shared/ui/Tabs";
import { formatBytes } from "../settings/storageHint";
import { CallFacts } from "./CallFacts";
import { RESPONSE_PREVIEW_BYTES, responsePreview } from "./responsePreview";
import styles from "./CallDetails.module.scss";

/// 请求级协议的展示值：对象按 JSON 显示，缺失用 `-`，不要印出 undefined。
function protocolValue(value: unknown): string {
  if (value === null || value === undefined || value === "") return "-";
  if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  try { return JSON.stringify(value); } catch { return String(value); }
}

function ProtocolSummary({ call, body }: { call: LlmCall; body: unknown }) {
  const object = body !== null && typeof body === "object" ? body as Record<string, unknown> : {};
  const reasoning = object.reasoning !== null && typeof object.reasoning === "object"
    ? object.reasoning as Record<string, unknown>
    : {};
  const tools = Array.isArray(object.tools) ? object.tools.length : call.tool_count;
  return <div className={styles.protocol}>
    <h4>{t("请求协议")}</h4>
    <dl>
      <div><dt>{t("思考强度")}</dt><dd><code>{protocolValue(reasoning.effort ?? call.reasoning_effort)}</code></dd></div>
      <div><dt>reasoning.summary</dt><dd><code>{protocolValue(reasoning.summary)}</code></dd></div>
      <div><dt>tool_choice</dt><dd><code>{protocolValue(object.tool_choice)}</code></dd></div>
      <div><dt>parallel_tool_calls</dt><dd><code>{protocolValue(object.parallel_tool_calls)}</code></dd></div>
      <div><dt>{t("消息 / 工具数")}</dt><dd>{call.message_count} / {tools}</dd></div>
    </dl>
  </div>;
}

/// 压缩触发方式的可读标签；未知值原样显示，后端加新触发方式时界面不会变成空白。
function compactionTriggerLabel(trigger: string) {
  switch (trigger) {
    case "preflight": return t("主动预检");
    case "context-overflow": return t("上游拒绝后补救");
    case "explicit": return t("手动压缩");
    default: return trigger;
  }
}

function LazyJsonEditor({ ariaLabel, source, detail }: { ariaLabel: string; source: unknown; detail?: boolean }) {
  const value = useMemo(() => JSON.stringify(source), [source]);
  return <JsonEditor ariaLabel={ariaLabel} value={value} readOnly detail={detail} />;
}

export function CallDetails({ detail }: { detail: CallDetail }) {
  const { call, request, cursor_trace: cursorTrace } = detail;
  const chunks = detail.response_chunks ?? [];
  const [showAllResponse, setShowAllResponse] = useState(false);

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
      <ProtocolSummary call={call} body={request?.body} />
      {request ? <>
        <div className={styles.meta}>{t("字节数")}：{request.byte_count}</div>
        <h4>{t("请求头")}</h4><LazyJsonEditor ariaLabel={t("请求头")} source={request.headers} />
        <h4>{t("请求体")}</h4><LazyJsonEditor ariaLabel={t("请求体")} source={request.body} detail />
      </> : <div className={styles.empty}>{t("未记录请求内容，请在「系统设置 → 详细记录」开启后重试。")}</div>}
    </section> },
    { value: "response", label: t("响应流"), content: <ResponseTabContent chunks={chunks} showAll={showAllResponse} onShowAll={() => setShowAllResponse(true)} /> },
  ];

  if (cursorTrace) tabs.push({ value: "cursor-trace", label: t("Cursor 追踪"), content: <section className={styles.section}>
      <div className={styles.meta}>
        Request ID：{cursorTrace.trace.request_id} · {t("工件数")}：{cursorTrace.artifacts.length}
      </div>
      <LazyJsonEditor
        ariaLabel={t("Cursor 追踪")}
        source={{ trace: cursorTrace.trace, artifacts: cursorTrace.artifacts }}
        detail
      />
    </section> });

  return <div className={styles.root}><Tabs items={tabs} /></div>;
}

function ResponseTabContent({ chunks, showAll, onShowAll }: {
  chunks: CallDetail["response_chunks"];
  showAll: boolean;
  onShowAll: () => void;
}) {
  const response = useMemo(() => responsePreview(chunks, { showAll }), [chunks, showAll]);
  return <section className={styles.section}>
    {chunks.length > 0 ? <>
      <div className={styles.meta}>{t("分块数")}：{chunks.length} · {t("字节数")}：{response.totalBytes}</div>
      {response.truncated && <div className={styles.meta}>
        <span>{t("仅显示前 {shown}（共 {total}）", { shown: formatBytes(response.shownBytes), total: formatBytes(response.totalBytes) })}</span>
        <Button size="small" onClick={onShowAll}>{t("显示全部")}</Button>
      </div>}
      <JsonEditor ariaLabel={t("响应流")} value={response.text} readOnly detail />
      {response.truncated && <small>{t("默认只渲染前 {limit}；点「显示全部」会一次性加载整段响应。", { limit: formatBytes(RESPONSE_PREVIEW_BYTES) })}</small>}
    </> : <div className={styles.empty}>{t("未记录响应内容，请在「系统设置 → 详细记录」开启后重试。")}</div>}
  </section>;
}
