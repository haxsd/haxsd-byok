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
