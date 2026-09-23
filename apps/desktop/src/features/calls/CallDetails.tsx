import type { CallDetail } from "../../shared/api";
import { JsonEditor } from "../../shared/ui/JsonEditor";
import { Tabs, type TabItem } from "../../shared/ui/Tabs";
import { CallFacts } from "./CallFacts";
import styles from "./CallDetails.module.scss";

export function CallDetails({ detail }: { detail: CallDetail }) {
  const { call, request, response_chunks: chunks, cursor_trace: cursorTrace } = detail;
  const responseBody = chunks.map((chunk) => chunk.data).join("");
  const responseBytes = chunks.reduce((total, chunk) => total + chunk.byte_count, 0);

  const tabs: TabItem[] = [
    { value: "call", label: t("调用信息"), content: <section className={styles.section}>
      <CallFacts call={call} />
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
        <div className={styles.meta}>{t("分块数")}：{chunks.length} · {t("字节数")}：{responseBytes}</div>
        <JsonEditor ariaLabel={t("响应流")} value={responseBody} readOnly detail />
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
