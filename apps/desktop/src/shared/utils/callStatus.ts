import type { StatusTone } from "../ui/StatusPill";

/**
 * 一次调用的状态：颜色和文字只在这里定义。
 *
 * 「调用」表格和调用详情页各自写过一份同样的映射，而且都把数据库里的英文状态
 * 原样显示在中文界面上——筛选条写着「成功 / 失败」，同一屏的每一行却写着
 * "completed"。两处现在共用这一个函数，改词也只改一处。
 */
const TONES: Record<string, StatusTone> = {
  completed: "ok",
  failed: "bad",
  error: "bad",
  cancelled: "warn",
  running: "info",
};

export function callStatusTone(status: string): StatusTone {
  return TONES[status] ?? "idle";
}

/** 未知状态保留原文：宁可露出英文，也不要给它安一个不准确的词。 */
export function callStatusLabel(status: string): string {
  switch (status) {
    case "completed": return t("成功");
    case "failed": return t("失败");
    case "error": return t("错误");
    case "cancelled": return t("已取消");
    case "running": return t("进行中");
    default: return status;
  }
}

/**
 * 一次调用的客户端来源。
 *
 * 这个字段以前有两套说法：调用页的筛选条写「内部调用」，展开行和详情页直接显示
 * 数据库里的 `internal`／`cursor`——同一屏里同一个字段，一处中文一处英文。这里
 * 收成一处：筛选条和明细都从这里取名字。
 *
 * 客户端名是固定产品名，不参与翻译；未知来源保留原文。
 */
export function callClientLabel(source: string): string {
  switch (source) {
    case "cursor": return "Cursor";
    case "devin": return "Devin";
    case "codex": return "Codex";
    case "claude": return "Claude";
    case "internal": return t("内部调用");
    default: return source;
  }
}

/**
 * 调用页来源筛选的取值顺序；新增客户端只改这一处。
 *
 * 与 `callClientLabel` 放一起，避免筛选项和表格/明细的称呼各自漂移。
 */
export const callClientSources = ["cursor", "devin", "codex", "claude", "internal"] as const;

/**
 * 路由列：客户端来源 + 路由类型。
 *
 * Claude 与 Codex/Devin 一样经由本机网关，必须有独立标签；落到通用 `BYOK` 会让
 * 三条不同的接入路径在表格里看起来是同一条。
 */
export function callRouteLabel(call: { client_source: string; route: string }): string {
  switch (call.client_source) {
    case "codex": return "Codex · BYOK";
    case "devin": return "Devin · BYOK";
    case "claude": return "Claude · BYOK";
    default: return call.route === "cursor_official" ? t("官方") : "BYOK";
  }
}
