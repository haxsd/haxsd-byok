/**
 * 更新失败分类：把更新器与 Windows 抛出的英文错误归到用户能据以行动的四类。
 *
 * 2026-10-08 实测的现场：安装包下载成功，`install()` 也"成功启动"，但 Windows
 * Smart App Control 在 `%TEMP%` 里把未签名的安装器拦下（CodeIntegrity 3077/3033 +
 * SAC 3118），应用重启回旧版本，界面只剩一句英文系统错误——用户完全看不出原因，
 * 也看不出下一步该换代理还是该改系统策略。
 */
export type UpdateFailureKind = "policy" | "permission" | "network" | "unknown";

export type UpdateFailure = {
  kind: UpdateFailureKind;
  /** 系统给的原始错误串；分类之后仍然保留，供悬停查看与转发给开发者。 */
  raw: string;
};

/** 系统策略拦下未签名的安装器（Code Integrity / Smart App Control）。 */
const POLICY_MARKERS = ["application control policy", "blocked this file", "os error 4551"];
/** 安装器没有写入权限。 */
const PERMISSION_MARKERS = ["os error 5", "access is denied"];
/** 清单或安装包没下载下来：本机 github.com 直连不稳，系统代理开关会变。 */
const NETWORK_MARKERS = ["403", "timed out", "dns", "connection"];

/** 分类只看错误原文；匹配顺序即优先级，系统策略要排在权限与网络前面。 */
export function classifyUpdateFailure(cause: unknown): UpdateFailure {
  const raw = cause instanceof Error ? cause.message : String(cause);
  const message = raw.toLowerCase();
  const kind: UpdateFailureKind = POLICY_MARKERS.some((marker) => message.includes(marker))
    ? "policy"
    : PERMISSION_MARKERS.some((marker) => message.includes(marker))
      ? "permission"
      : NETWORK_MARKERS.some((marker) => message.includes(marker))
        ? "network"
        : "unknown";
  return { kind, raw };
}

/**
 * 「安装器启动了，但重启后版本没变」这一类的代表值：没有系统原文可引用（唯一的
 * 证据是版本号没变），用户侧的原因按系统拦截解释——实测现场就是 Smart App Control
 * 拦下了未签名的安装包。
 */
export const NOT_APPLIED: UpdateFailure = { kind: "policy", raw: "" };

/** 分类对应的原因与下一步；在渲染路径里调用，所以切换语言后会跟着变。 */
export function describeUpdateFailure(failure: UpdateFailure): string {
  if (failure.kind === "policy") {
    return t("安装包被 Windows 系统策略拦下了（Smart App Control／代码完整性）；未签名的安装器会被直接拒绝执行。要装这个版本，需要关闭“智能应用控制”（不可逆），或改用带签名的安装包。");
  }
  if (failure.kind === "permission") {
    return t("安装程序没有写入权限。请手动下载安装包并以管理员身份运行，或检查安装目录的权限。");
  }
  if (failure.kind === "network") {
    return t("请求没有走通，通常是本机到 github.com 的连接或系统代理不稳定。请换直连或代理后重试，或手动下载安装包。");
  }
  return failure.raw
    ? t("错误原文：{detail}。请把这条信息发给开发者。", { detail: failure.raw })
    : t("出现了未归类的错误，请把这条信息发给开发者。");
}
