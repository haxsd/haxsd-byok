import { useEffect, useState } from "react";
import { api, type LlmCall } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { callClientLabel } from "../../shared/utils/callStatus";
import { useAppStore } from "../../shared/store/appStore";
import { formatCompactInteger } from "../../shared/utils/numberFormat";
import { formatDuration } from "../../shared/utils/relativeTime";
import { copyErrorDiagnostic, sanitizeDiagnosticText } from "./errorDiagnostics";
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
    [t("上游调用序号"), numberText(call.provider_call_index)],
    ["Model Hash", call.model_hash ?? "-"],
    ["Created At", Number.isFinite(call.created_at_ms) ? `${call.created_at_ms} · ${new Date(call.created_at_ms).toLocaleString()}` : "-"],
  ] : [];
  const timing: Array<[string, string]> = developerMode ? [
    [t("客户端来源"), callClientLabel(call.client_source)],
    ["TTFB", formatDuration(call.ttfb_ms)],
    ["TTFR", formatDuration(call.ttfr_ms)],
    ["TTFT", formatDuration(call.ttft_ms)],
    [t("总耗时"), formatDuration(call.duration_ms)],
    ["HTTP", numberText(call.http_status)],
    ["Finish Reason", call.finish_reason ?? "-"],
  ] : [
    [t("客户端来源"), callClientLabel(call.client_source)],
    [t("总耗时"), formatDuration(call.duration_ms)],
  ];
  const usage: Array<[string, string]> = [
    [t("输入 Token"), numberText(call.input_tokens)],
    [t("输出 Token"), numberText(call.output_tokens)],
    [t("缓存读取"), numberText(call.cache_read_tokens)],
    [t("缓存写入"), numberText(call.cache_write_tokens)],
    [t("推理 Token"), numberText(call.reasoning_tokens)],
    ...(developerMode ? [[t("消息 / 工具数"), `${numberText(call.message_count)} / ${numberText(call.tool_count)}`] as [string, string]] : []),
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
  // 旧记录可能只有状态没有 error_kind/error_message；错误块仍要给出定位编号。
  const hasError = Boolean(call.error_kind?.trim() || call.error_message?.trim() || call.status === "failed" || call.status === "error");
  return <div className={styles.facts}>
    {identity.length > 0 && <FactGroup title={t("标识")} rows={identity} />}
    <FactGroup title={t("耗时与结果")} rows={timing} />
    <FactGroup title={t("Token（合计 {tokens}）", { tokens: formatCompactInteger(tokens) })} rows={usage} />
    {context.length > 0 && <FactGroup title={t("请求与上游")} rows={context} />}
    {hasError && <ErrorDiagnostic call={call} developerMode={developerMode} />}
  </div>;
}

/**
 * 错误块：稳定错误码、完整定位编号和脱敏后的原因。
 *
 * 普通模式以前只能看到原始 kind 和一段原因，拿不到可以贴在问题里的编号。复制动作
 * 只带固定字段（不发请求体/响应/聊天），并在浏览器打开的详情页里走 navigator.clipboard。
 */
function ErrorDiagnostic({ call, developerMode }: { call: LlmCall; developerMode: boolean }) {
  const [appVersion, setAppVersion] = useState<string | null>(null);
  const [copyState, setCopyState] = useState<"idle" | "copying" | "copied" | "failed">("idle");
  const [copyError, setCopyError] = useState("");

  // 版本来自 /api/app-info（桌面壳启动时写入）。取不到就保持 unknown，不阻断复制。
  useEffect(() => {
    let cancelled = false;
    void api.appInfo()
      .then((info) => { if (!cancelled) setAppVersion(info.version); })
      .catch(() => { /* 版本只是诊断字段；失败时仍复制其余现场。 */ });
    return () => { cancelled = true; };
  }, []);

  const code = call.error_kind?.trim() || t("未知");
  const reason = sanitizeDiagnosticText(call.error_message);
  const httpStatus = typeof call.http_status === "number" && Number.isFinite(call.http_status) && call.http_status > 0
    ? String(call.http_status)
    : null;

  const copy = async () => {
    setCopyState("copying");
    setCopyError("");
    const result = await copyErrorDiagnostic(call, appVersion, api.copyCursorText);
    if (result.ok) {
      setCopyState("copied");
    } else {
      setCopyError(result.reason);
      setCopyState("failed");
    }
  };

  return <div className={styles.errorGroup}>
    <span className={styles.factTitle}>{t("错误")}</span>
    <dl className={styles.errorFacts}>
      <div><dt>{t("错误码")}</dt><dd><code>{code}</code></dd></div>
      <div><dt>{t("定位编号")}</dt><dd><code>{call.call_id || "-"}</code></dd></div>
      {httpStatus && <div><dt>HTTP</dt><dd>{httpStatus}</dd></div>}
    </dl>
    {reason && <p className={styles.errorReason}>{reason}</p>}
    {!developerMode && call.provider_url && <p className={styles.errorUpstream}>
      {t("上游地址：{url}", { url: sanitizeDiagnosticText(call.provider_url) })}
    </p>}
    <div className={styles.errorActions}>
      <Button size="small" disabled={copyState === "copying"} onClick={() => void copy()}>
        {copyState === "copied" ? t("已复制") : t("复制错误诊断")}
      </Button>
      {copyState === "failed" && <small role="alert" className={styles.copyFailure}>
        {t("复制失败：{reason}", { reason: copyError || t("未知") })}
      </small>}
    </div>
  </div>;
}

/**
 * 可选数值的统一呈现。
 *
 * 只判断 `=== null` 是不够的：服务端少给一个字段时值是 undefined，卡片上就会印出
 * "undefined"/"Invalid Date"——看起来像应用坏了，而不是这一行缺数据。
 */
function numberText(value: number | null | undefined) {
  return value === null || value === undefined || !Number.isFinite(value) ? "-" : String(value);
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
