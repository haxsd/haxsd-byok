import type { LlmCall } from "../../shared/api";
import { useAppStore } from "../../shared/store/appStore";
import { formatCompactInteger } from "../../shared/utils/numberFormat";
import { formatDuration } from "../../shared/utils/relativeTime";
import styles from "./CallFacts.module.scss";

/**
 * 一次调用的全部字段，按「谁是谁 / 快不快 / 花了多少 / 打到哪」分四组。
 *
 * 表格原来把它们摊平成 30 列（4180px 宽），详情页把它们塞进一张两列的长表。
 * 同一个集合在这里定义一次，两处共用：展开行和详情页说的是同一件事，
 * 字段顺序和分组也必须一致，否则读完表格再打开详情会以为看的是两次不同的调用。
 *
 * 字段按 B1 的三档分法：保留项（耗时、Token 构成、错误信息）回答"发生了什么、
 * 花了多少"；内部标识（Call/Run/Conversation ID、Model Hash、原始毫秒）、协议级分解
 * （TTFB/TTFR/TTFT/HTTP/Finish Reason）、上游拓扑（上游类型/地址、最终请求、请求地址、
 * 思考强度、Fast）与消息/工具数只在开发者模式下显示。开关关闭时整组不渲染；
 * 报错时上游地址仍留在错误块里——"打到哪个上游失败"是排查的第一步。
 */
export function CallFacts({ call }: { call: LlmCall }) {
  const { developerMode } = useAppStore();
  const tokens = call.total_tokens ?? (call.input_tokens ?? 0) + (call.output_tokens ?? 0);
  const identity: Array<[string, string]> = developerMode ? [
    ["Call ID", call.call_id],
    ["Run ID", call.run_id],
    ["Conversation ID", call.conversation_id],
    [t("上游调用序号"), String(call.provider_call_index)],
    ["Model Hash", call.model_hash ?? "-"],
    ["Created At", `${call.created_at_ms} · ${new Date(call.created_at_ms).toLocaleString()}`],
  ] : [];
  const timing: Array<[string, string]> = developerMode ? [
    ["TTFB", formatDuration(call.ttfb_ms)],
    ["TTFR", formatDuration(call.ttfr_ms)],
    ["TTFT", formatDuration(call.ttft_ms)],
    [t("总耗时"), formatDuration(call.duration_ms)],
    ["HTTP", call.http_status === null ? "-" : String(call.http_status)],
    ["Finish Reason", call.finish_reason ?? "-"],
  ] : [
    [t("总耗时"), formatDuration(call.duration_ms)],
  ];
  const usage: Array<[string, string]> = [
    [t("输入 Token"), call.input_tokens === null ? "-" : String(call.input_tokens)],
    [t("输出 Token"), call.output_tokens === null ? "-" : String(call.output_tokens)],
    [t("缓存读取"), call.cache_read_tokens === null ? "-" : String(call.cache_read_tokens)],
    [t("缓存写入"), call.cache_write_tokens === null ? "-" : String(call.cache_write_tokens)],
    [t("推理 Token"), call.reasoning_tokens === null ? "-" : String(call.reasoning_tokens)],
    ...(developerMode ? [[t("消息 / 工具数"), `${call.message_count} / ${call.tool_count}`] as [string, string]] : []),
  ];
  const context: Array<[string, string]> = developerMode ? [
    [t("上游类型"), call.provider_type],
    [t("上游地址"), call.provider_url],
    [t("最终请求"), call.request_type],
    [t("请求地址"), call.request_url],
    [t("思考强度"), call.reasoning_effort ?? "-"],
    ["Fast", call.fast === null ? "-" : call.fast ? t("是") : t("否")],
    [t("详细记录"), call.detailed ? t("是") : t("否")],
  ] : [];
  return <div className={styles.facts}>
    {identity.length > 0 && <FactGroup title={t("标识")} rows={identity} />}
    <FactGroup title={t("耗时与结果")} rows={timing} />
    <FactGroup title={t("Token（合计 {tokens}）", { tokens: formatCompactInteger(tokens) })} rows={usage} />
    {context.length > 0 && <FactGroup title={t("请求与上游")} rows={context} />}
    {call.error_message && <div className={styles.errorGroup}>
      <span className={styles.factTitle}>{call.error_kind ?? t("错误")}</span>
      <p>{call.error_message}</p>
      {!developerMode && call.provider_url && <p>{t("上游地址：{url}", { url: call.provider_url })}</p>}
    </div>}
  </div>;
}

function FactGroup({ title, rows }: { title: string; rows: Array<[string, string]> }) {
  return <div className={styles.factGroup}>
    <span className={styles.factTitle}>{title}</span>
    <dl>{rows.map(([label, value]) => <div key={label}>
      <dt>{label}</dt>
      <dd title={value}>{value || "-"}</dd>
    </div>)}</dl>
  </div>;
}
