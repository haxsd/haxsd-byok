import type { LlmCall } from "../../shared/api";

/**
 * 复制诊断时允许出现的字段都从这里取：调用记录里的固定字段，加上当前运行版本。
 *
 * 刻意不接收 `CallDetail`：请求体、响应分块、对话消息都不在复制范围内。历史上没有
 * 按调用保存应用版本，所以发生版本只能写 `unknown`，不能拿当前版本冒充。
 */
export function buildErrorDiagnostic(call: LlmCall, currentAppVersion: string | null): string {
  const protocol = sanitizeDiagnosticText(call.request_type) || sanitizeDiagnosticText(call.provider_type) || "unknown";
  return [
    "haxsd byok error diagnostic",
    `client: ${sanitizeDiagnosticText(call.client_source) || "unknown"}`,
    `time: ${formatDiagnosticTime(call.created_at_ms)}`,
    `model: ${sanitizeDiagnosticText(call.model_id) || "unknown"}`,
    `protocol: ${protocol}`,
    `code: ${sanitizeDiagnosticText(call.error_kind) || "unknown"}`,
    `call_id: ${sanitizeDiagnosticText(call.call_id) || "unknown"}`,
    `known_http_status: ${knownHttpStatus(call.http_status)}`,
    // 现有记录没有“错误发生时的应用版本”字段；写 unknown 比写当前版本诚实。
    "occurred_app_version: unknown",
    `current_app_version: ${sanitizeDiagnosticText(currentAppVersion) || "unknown"}`,
    `reason: ${sanitizeDiagnosticText(call.error_message) || "unknown"}`,
  ].join("\n");
}

export type DiagnosticCopyResult = { ok: true } | { ok: false; reason: string };

/**
 * 复制诊断并保留失败原因，界面据此显示明确的失败反馈。
 *
 * 版本在组件挂载时已经取好，这里不再等待网络：浏览器剪贴板需要用户手势，
 * 点击后到 writeText 之间不能再插入一次请求。
 */
export async function copyErrorDiagnostic(
  call: LlmCall,
  currentAppVersion: string | null,
  copyText: (text: string) => Promise<void>,
): Promise<DiagnosticCopyResult> {
  try {
    await copyText(buildErrorDiagnostic(call, currentAppVersion));
    return { ok: true };
  } catch (cause) {
    return { ok: false, reason: cause instanceof Error ? cause.message : String(cause) };
  }
}

/** 已知 HTTP 状态：本地没拿到就是未知，不用 0 或当前时间猜。 */
export function knownHttpStatus(value: number | null | undefined): string {
  return typeof value === "number" && Number.isFinite(value) && value > 0 ? String(value) : "unknown";
}

function formatDiagnosticTime(value: number | null | undefined): string {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) return "unknown";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "unknown" : date.toISOString();
}

const MAX_DIAGNOSTIC_TEXT = 2_000;
const REDACTED = "[redacted]";

/**
 * 错误原因可能来自旧记录或上游正文，复制前再脱敏一次。
 *
 * 只处理可识别的凭据形态：URL 里的 userinfo、查询参数中的令牌、Authorization 等
 * 请求头、Bearer、常见 API key 前缀。普通 URL、模型名和错误文字保持可读；超长文本
 * 截断，避免把整段响应正文带进剪贴板。
 */
export function sanitizeDiagnosticText(value: string | null | undefined): string {
  if (typeof value !== "string" || value === "") return "";
  let text = value.replace(/\r\n?/g, "\n");
  // https://user:pass@host/... -> https://[redacted]@host/...
  text = text.replace(/\b([a-z][a-z0-9+.-]*:\/\/)([^\s/@]+)@/gi, `$1${REDACTED}@`);
  // JSON 里的敏感键先处理，避免后面的请求头规则吞掉同一行的其它字段。
  text = text.replace(
    /("(?:api[_-]?key|access[_-]?token|refresh[_-]?token|token|secret|password|authorization)"\s*:\s*")([^"]*)(")/gi,
    `$1${REDACTED}$3`,
  );
  // Authorization / x-api-key 等整行请求头；不匹配 JSON 引号内的同名键。
  text = text.replace(
    /(^|[^"\w?&#])(authorization|proxy-authorization|x-api-key|api[_-]?key|apikey|x-goog-api-key)\s*[:=][^\n]*/gi,
    `$1$2: ${REDACTED}`,
  );
  text = text.replace(/\bBearer\s+[A-Za-z0-9._~+/=-]+/gi, `Bearer ${REDACTED}`);
  text = text.replace(
    /([?&#](?:api[_-]?key|apikey|access[_-]?token|refresh[_-]?token|token|secret|password|signature|sig|auth|key)=)[^&\s#]+/gi,
    `$1${REDACTED}`,
  );
  text = text.replace(
    /\b(?:sk-[A-Za-z0-9_-]{8,}|ghp_[A-Za-z0-9]{8,}|github_pat_[A-Za-z0-9_]{8,}|xox[baprs]-[A-Za-z0-9-]{8,}|AKIA[0-9A-Z]{12,}|AIza[0-9A-Za-z_-]{20,})\b/g,
    REDACTED,
  );
  text = text.replace(/\b(cookie|set-cookie)\s*:\s*[^\n]+/gi, `$1: ${REDACTED}`);
  text = text.replace(/[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/g, "");
  return text.length > MAX_DIAGNOSTIC_TEXT ? `${text.slice(0, MAX_DIAGNOSTIC_TEXT)}…` : text;
}
