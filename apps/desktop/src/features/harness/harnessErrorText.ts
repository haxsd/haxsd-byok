// 接入页把服务端错误翻成界面上的说法。
//
// 管理接口的报错是英文原句（服务端不知道界面语言，见 server/src/devin/host_detect.rs
// 的说明），直接显示就是中文界面里的一行英文报错，而且前面还挂着 `configuration error:`
// 这类实现细节。这里按已知原因给出用户能照着做的说法；认不出的原因保留原句，但去掉
// 前缀——前缀是实现细节，不是给用户看的信息。
//
// 表里的 match 是服务端原句里的稳定片段：改服务端文案时这里要跟着改，测试会盯住
// 这些片段仍然被翻译。

import { RequestTimeoutError } from "../../shared/api";

interface KnownReason {
  /** 服务端原句里的片段，按最长/最具体优先排列。 */
  match: string;
  text: (raw: string) => string;
}

/**
 * 服务端把"备份在哪"统一放在句子末尾的 `backup: <路径>`（见 server/src/devin/host_patch.rs
 * 与 server/src/control/devin.rs）。翻译成中文时要把这段路径留下来：用户读到"备份和收据
 * 不一致"却看不到路径，就没法照做下一步。
 */
function backupHint(raw: string): string {
  const match = raw.match(/backup:\s*(.+)$/m);
  return match ? t("（备份：{path}）", { path: match[1].trim() }) : "";
}

const knownReasons: KnownReason[] = [
  // ---- Devin 宿主接入 ----
  { match: "Devin installation not found", text: () => t("没有找到 Devin 安装。请在高级里填写宿主文件路径：安装目录下的 extension.js。") },
  { match: "is not a Devin host file", text: () => t("这个路径不是 Devin 的宿主文件。请填写安装目录，或 extension.js 的完整路径。") },
  { match: "Devin host file path must be absolute", text: () => t("宿主文件路径要填绝对路径，例如 D:\\Devin。") },
  { match: "enabling the Devin gateway is required", text: () => t("先启用并保存 Devin 网关，再应用宿主补丁。") },
  { match: "no usable Devin host restore receipt", text: (raw) => t("没有可用的恢复记录：这个文件不是通过本应用打的补丁，或补丁记录已被改动。文件保持原样。") + backupHint(raw) },
  { match: "Devin host file is already patched", text: (raw) => t("宿主文件已经是打过补丁的版本。请先恢复原文件，再应用这次的补丁。") + backupHint(raw) },
  { match: "changed after patch", text: (raw) => t("宿主文件在打补丁之后又被改过，为避免覆盖你的修改，恢复已拒绝。") + backupHint(raw) },
  { match: "backup SHA-256 mismatch", text: (raw) => t("备份文件与打补丁时记下的内容不一致，恢复已拒绝。") + backupHint(raw) },
  { match: "backup is not a recognized clean version", text: (raw) => t("备份不是可识别的原版文件，恢复已拒绝。") + backupHint(raw) },
  { match: "is unavailable", text: (raw) => t("打补丁时留下的备份文件找不到了，恢复已拒绝；文件保持原样。") + backupHint(raw) },
  { match: "is missing or ambiguous", text: () => t("宿主文件版本不匹配：要改写的锚点缺失或出现多次，补丁已拒绝。") },
  { match: "is missing; patch refused", text: () => t("宿主文件版本不匹配：要改写的锚点没找到，补丁已拒绝。") },
  { match: "has a partial or unknown patch", text: () => t("宿主文件里已有部分补丁痕迹，本应用不动它，避免叠加出无法恢复的状态。") },
  { match: "does not match all supported endpoint anchors", text: () => t("宿主文件与受支持的版本不一致，无法自动改写。") },
  { match: "cannot read explicit Devin host file", text: (raw) => t("读不到这个宿主文件，请检查路径与文件权限。") + backupHint(raw) },
  { match: "cannot read the Devin client", text: () => t("读不到 Devin 客户端文件，请检查上面的路径。") },
  { match: "not UTF-8", text: () => t("宿主文件不是 UTF-8 文本，无法安全改写。") },
  // ---- Devin 设置 ----
  { match: "Devin model UID must not be empty", text: () => t("模型映射要填 Devin 的模型标识。") },
  { match: "duplicate Devin model UID", text: () => t("同一个 Devin 模型标识只能有一条映射。") },
  { match: "duplicate Devin route ID", text: () => t("同一条映射里的候选路由不能重名。") },
  { match: "Devin context compression binding", text: () => t("上下文压缩映射不能带候选路由。") },
  { match: "Devin active route", text: () => t("当前路由必须存在且处于启用状态。") },
  { match: "must include a cursor-byok model hash", text: () => t("每条映射和每条路由都要选择一个模型库里的模型。") },
  { match: "Devin ports must be distinct", text: () => t("三个端口不能重复。") },
  { match: "port must be non-zero", text: () => t("端口不能为 0。") },
  // 保存 Devin 端口时的准入检查（server/src/control/ports.rs）：白话说明占用者是谁。
  { match: "is already used by haxsd byok as the management service port", text: () => t("这个端口是本应用的管理服务端口。给网关换一个端口，或先改服务端口。") },
  { match: "is already used by haxsd byok as the Cursor proxy port", text: () => t("这个端口是本应用的 Cursor 代理端口。给网关换一个端口，或先改代理端口。") },
  { match: "is already used by haxsd byok as the Codex gateway port", text: () => t("这个端口是本应用的 Codex 网关端口。给网关换一个端口，或先改 Codex 端口。") },
  { match: "is already in use by another program on this machine", text: () => t("这个端口已被本机其它程序占用。关掉它，或给网关换一个端口。") },
  { match: "cannot be listened on by this app", text: () => t("这个端口本应用绑不上（通常是权限或系统保留端口）。给网关换一个端口。") },
  { match: "Devin upstream API URL must not be empty", text: () => t("上游 API 地址不能为空。") },
  { match: "Devin upstream API URL must be an absolute", text: () => t("上游 API 地址要填完整的 http(s) 地址。") },
  // 应用宿主补丁前的监听检查（server/src/devin/gateway.rs）：补丁会把客户端指向三个端口，
  // 网关只在启动时绑一次，所以"还没监听"和"监听的不是设置里那几个"要分开说。
  { match: "gateway is not listening", text: () => t("Devin 网关还没有开始监听：先启用并保存网关，再重启软件，然后回到这里应用补丁。") },
  { match: "not on the configured", text: () => t("Devin 网关正在监听的端口与当前设置不一致：重启软件让它按保存的端口重新监听，再来应用补丁。") },
  // ---- Codex 网关 ----
  { match: "enable the Codex gateway before applying configuration", text: () => t("先启用 Codex 网关，再应用配置。") },
  { match: "restore the applied Codex configuration before changing", text: () => t("先撤销接管，再修改 Codex 网关设置。") },
  { match: "Codex routing conflicts with external configuration", text: () => t("目标目录里已有其他 provider，本机网关接管不了它；保留原文件，不强制覆盖。") },
  { match: "configure the actual context window", text: () => t("模型没有填真实的上下文窗口。请在模型库里补齐后再应用。") },
  { match: "configure the model context window", text: () => t("默认模型没有填上下文窗口。请在模型库里补齐后再应用。") },
  { match: "select a default Codex model", text: () => t("先选择默认模型。") },
  { match: "unknown Codex model", text: () => t("选中的模型已不在模型库里，请重新选择。") },
  { match: "Codex has active requests", text: () => t("Codex 正在处理请求，等它空闲后再改配置。") },
  { match: "Codex model selections must be non-empty and unique", text: () => t("可用模型不能为空，也不能重复。") },
  { match: "Codex port must be non-zero", text: () => t("端口不能为 0。") },
  { match: "Codex home must be an absolute path", text: () => t("配置目录要填绝对路径。") },
  // ---- 插件模型 ----
  { match: "is disabled; enable it in the plugin", text: () => t("这个插件模型已经停用。到插件页的「模型管理」里启用它，或改用别的模型。") },
];

/** 服务端错误前缀：`configuration error: …`、`io error: …` 等。 */
const errorPrefix = /^[a-z][a-z ]* error: /;

/**
 * 界面要显示给用户的错误说法。
 *
 * 认得出的原因给中文，认不出的原因保留服务端原句（去掉前缀），这样排查时仍能看到真实
 * 原因，而不是一律塌成"操作失败"。
 */
export function clientErrorText(cause: unknown): string {
  const raw = cause instanceof Error ? cause.message : String(cause ?? "");
  const text = raw.trim();
  if (!text) return t("操作失败");
  for (const reason of knownReasons) {
    if (text.includes(reason.match)) return reason.text(text);
  }
  return text.replace(errorPrefix, "");
}

/**
 * 写操作失败时的说法。
 *
 * 写超时和读超时不是一回事：读超时就是没读到，写超时是"服务端可能已经保存了，也可能没有"。
 * 报成普通的一句"管理请求超时"会让人以为没生效而重复提交（第二次可能被服务端以"已经存在"
 * 拒绝，甚至写出两份）。所以这一类失败必须说清"结果未知"，并指向刷新确认。
 */
export function writeFailureText(cause: unknown): string {
  if (cause instanceof RequestTimeoutError) {
    return t("这次写入还没有结果：服务端可能已经保存，也可能没有。先刷新确认，再决定要不要重试。");
  }
  return clientErrorText(cause);
}
