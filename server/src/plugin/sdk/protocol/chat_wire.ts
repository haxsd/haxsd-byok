/** Chat wire 的共享判定：与 Rust 的 `provider/wire/chat.rs` 使用同一份用例表。 */

export type ChatFinish = "stop" | "length" | "tool-use" | "content-filter" | "failed";

export function chatFinish(reason: string): ChatFinish {
  const value = reason.toLowerCase();
  if (value === "tool_calls" || value === "function_call") return "tool-use";
  if (value === "length") return "length";
  if (value === "content_filter") return "content-filter";
  if (value.includes("fail") || value.includes("error") || value.includes("cancel")) {
    return "failed";
  }
  return "stop";
}

/** 标识符（call_id、函数名）的分片归并：重复只可能是旧值或完整重发。 */
export function mergeIdentifier(target: string, fragment: string): string {
  if (target === fragment) return target;
  if (target.startsWith(fragment)) return target;
  if (fragment.startsWith(target)) return fragment;
  return target + fragment;
}

/** 工具参数是流式 JSON：短分片本身就是内容，只有整串重发才去重。 */
export function mergeArguments(target: string, fragment: string): string {
  if (target === fragment) return target;
  if (fragment.startsWith(target)) return fragment;
  return target + fragment;
}
