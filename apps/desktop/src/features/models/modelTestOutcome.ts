/**
 * 连通性测试失败原因的分类（11/P3-2）。
 *
 * 服务端在**超时后立刻收尾，并把这次测试算作取消**（`control/service.rs`：
 * `cancellation.cancel()` + `"model connectivity test timed out after {n} seconds"`，
 * 45 秒上限）。所以界面上说"超时"是不对的：那听起来像"还在跑、再等等"，而且与服务端记下的
 * 结论（取消）不一致——用户点开详情看到"超时"，去调用记录里却找不到一次失败的测试。
 *
 * 结论按"已取消"显示，"超时上限"只作为原因放进详情。上限数字从服务端原句里取，认不出就
 * 不印数字（宁可不提，也不编一个 45）。
 */

/** 服务端超时原句里的固定前缀（`connectivity_test_timeout_error`）。 */
const TIMEOUT_SENTENCE = "model connectivity test timed out after";

export type ModelTestEnd =
  /** 结论是取消：`reason` 为原因（服务端超时上限），没有原因就是用户自己点的取消。 */
  | { status: "cancelled"; reason: string | null }
  | { status: "failed"; reason: string };

/**
 * 服务端原句是不是"连通性测试超时"。
 *
 * 只认这一句：`errorText` 的其它映射（离线、写超时……）都不该被归到"已取消"。
 */
export function isConnectivityTimeout(cause: unknown): boolean {
  return messageOf(cause).toLowerCase().includes(TIMEOUT_SENTENCE);
}

/** 从服务端原句里取超时上限（秒）；认不出数字时给 null，界面据此不印数字。 */
export function connectivityTimeoutSeconds(cause: unknown): number | null {
  const matched = messageOf(cause).match(/timed out after (\d+)\s*seconds?/i);
  if (!matched) return null;
  const seconds = Number(matched[1]);
  return Number.isFinite(seconds) && seconds > 0 ? seconds : null;
}

/** 超时收尾后界面上要显示的原因（放在详情/提示里，摘要只写"已取消"）。 */
export function connectivityTimeoutReason(cause: unknown): string {
  const seconds = connectivityTimeoutSeconds(cause);
  return seconds === null
    ? t("超过连通性测试的时间上限，已按取消收尾；这次测试没有算作失败。")
    : t("超过 {seconds} 秒的连通性测试上限，已按取消收尾；这次测试没有算作失败。", { seconds });
}

/**
 * 把一次失败的连通性测试归类。
 *
 * `format` 是调用点自己的失败格式化（模型库与插件模型用的不是同一个），分类只负责把"超时"
 * 从失败里摘出来——它其实是一次取消。
 */
export function classifyModelTestFailure(cause: unknown, format: (cause: unknown) => string): ModelTestEnd {
  if (isConnectivityTimeout(cause)) return { status: "cancelled", reason: connectivityTimeoutReason(cause) };
  return { status: "failed", reason: format(cause) };
}

function messageOf(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
