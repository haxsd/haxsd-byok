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
 * 服务端把"备份在哪"统一放在句子末尾的 `backup: <路径>`（见 server/src/devin/host_config.rs
 * 与 server/src/control/devin_config.rs）。翻译成中文时要把这段路径留下来：用户读到"备份和收据
 * 不一致"却看不到路径，就没法照做下一步。
 */
function backupHint(raw: string): string {
  const match = raw.match(/backup:\s*(.+)$/m);
  return match ? t("（备份：{path}）", { path: match[1].trim() }) : "";
}

const knownReasons: KnownReason[] = [
  // ---- Devin 宿主接入 ----
  { match: "Devin installation not found", text: () => t("没有找到 Devin 安装。请先安装或启动一次 Devin；如果装在自定义位置，请在高级里填写安装目录。") },
  { match: "Devin user settings directory not found", text: () => t("没有找到 Devin 用户设置目录。先启动一次 Devin，或在高级里填写 settings.json 的绝对路径。") },
  { match: "Devin settings path must be absolute", text: () => t("设置文件路径要填绝对路径，例如 C:\\Users\\你\\AppData\\Roaming\\Devin。") },
  { match: "is not a Devin settings.json file", text: () => t("这个路径不是 Devin 的 settings.json。请填写 Devin 数据目录，或 User\\settings.json 的完整路径。") },
  { match: "is not a Devin settings file or data directory", text: () => t("这个路径不是 Devin 的设置文件或数据目录。留空可自动探测，也可以填 User\\settings.json。") },
  { match: "Devin installation path must be absolute", text: () => t("Devin 安装路径要填绝对路径，例如 D:\\devin\\Devin。") },
  { match: "is not a Devin extension.js file", text: () => t("这个路径不是 Devin 的 extension.js。请填写安装目录，或其中的 extension.js。") },
  { match: "is not a Devin installation directory", text: () => t("这个路径不是 Devin 安装目录。请检查安装位置，或直接填写其中的 extension.js。") },
  { match: "Devin installation path not found", text: () => t("找不到这个 Devin 安装路径，请检查路径是否仍然存在。") },
  { match: "Devin is still running", text: () => t("Devin 还在运行。请先完全退出 Devin，再写入或恢复设置；文件没有被改动。") },
  { match: "enable the Devin gateway before configuring Devin", text: () => t("先启用并保存 Devin 网关，再写入 Devin 设置。") },
  { match: "no usable Devin settings restore receipt", text: (raw) => t("没有可用的恢复记录：设置不是通过本应用写入的，或记录已被改动。文件保持原样。") + backupHint(raw) },
  { match: "Devin settings changed after this application configured them", text: (raw) => t("设置在本应用写入之后又被改过，为避免覆盖你的修改，恢复已拒绝。") + backupHint(raw) },
  { match: "Devin settings backup SHA-256 mismatch", text: (raw) => t("备份文件与写入设置时记下的内容不一致，恢复已拒绝。") + backupHint(raw) },
  { match: "Devin settings backup", text: (raw) => t("写入设置时留下的备份文件找不到了，恢复已拒绝；文件保持原样。") + backupHint(raw) },
  { match: "Devin settings already point at a local gateway", text: (raw) => t("Devin 设置已经指向一个本机网关。请先恢复之前的设置，再写入当前配置。") + backupHint(raw) },
  { match: "Devin settings contain only part of the gateway endpoints", text: () => t("Devin 设置里只有一部分网关地址。请先补全或删除这两项，再写入当前配置。") },
  { match: "Devin settings already define non-local or invalid API endpoints", text: () => t("Devin 设置已经指向其它地址。请先恢复原来的值，再写入本机网关。") },
  { match: "Devin settings file is not valid JSONC", text: () => t("Devin 的设置文件不是有效的 JSONC；本应用不会覆盖它，请先修正文件。") },
  { match: "Devin settings file is not a JSON object", text: () => t("Devin 的设置文件顶层不是对象；本应用不会覆盖它，请先修正文件。") },
  { match: "cannot read Devin settings file", text: (raw) => t("读不到 Devin 的设置文件，请检查路径与文件权限，并确认 Devin 已退出。") + backupHint(raw) },
  { match: "cannot read the Devin client", text: () => t("读不到 Devin 客户端文件，请检查安装是否完整。") },
  { match: "Devin settings value offsets", text: () => t("设置文件的属性位置无法安全解析；本应用没有改动它。") },
  { match: "not UTF-8", text: () => t("文件不是 UTF-8 文本，无法安全改写。") },
  // ---- Devin 设置 ----
  { match: "Devin model UID must not be empty", text: () => t("模型映射要填 Devin 的模型标识。") },
  { match: "duplicate Devin model UID", text: () => t("同一个 Devin 模型标识只能有一条映射。") },
  { match: "duplicate Devin route ID", text: () => t("同一条映射里的候选路由不能重名。") },
  { match: "Devin context compression binding", text: () => t("上下文压缩映射不能带候选路由。") },
  { match: "Devin active route", text: () => t("当前路由必须存在且处于启用状态。") },
  { match: "Devin default model must be selected", text: () => t("先选择一个默认模型，再保存并启用 Devin 网关。") },
  { match: "Devin default model UID not found", text: () => t("默认模型映射已不存在，请重新选择。") },
  { match: "Devin default model must be enabled", text: () => t("默认模型映射已停用，请重新选择。") },
  { match: "Devin default model must be a standard binding", text: () => t("默认模型只能选标准映射，不能选上下文压缩。") },
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
  // 写入 Devin 设置前的监听检查（server/src/devin/gateway.rs）：设置会把客户端指向两个端口，
  // 网关只在启动时绑一次，所以"还没监听"和"监听的不是设置里那几个"要分开说。
  { match: "gateway is not listening", text: () => t("Devin 网关还没有开始监听：先启用并保存网关，再重启软件，然后回到这里写入设置。") },
  { match: "not on the configured", text: () => t("Devin 网关正在监听的端口与当前设置不一致：重启软件让它按保存的端口重新监听，再来写入设置。") },
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
  // ---- Claude 网关 ----
  { match: "Claude port must be non-zero", text: () => t("端口不能为 0。") },
  { match: "select a default Claude model before enabling the gateway", text: () => t("先在网关里选择默认模型，再启用。") },
  { match: "Claude has active requests", text: () => t("Claude 正在处理请求，等它空闲后再改配置。") },
  { match: "unknown Claude", text: () => t("选中的模型已不在模型库里，请重新选择。") },
  { match: "model must not be blank", text: () => t("模型映射不能为空；留空时请选择「使用默认模型」。") },
  { match: "is already used by haxsd byok as the Claude gateway port", text: () => t("这个端口是本应用的 Claude 网关端口。给网关换一个端口，或先改 Claude 端口。") },
  { match: "is already used by haxsd byok as the Devin", text: () => t("这个端口是本应用的 Devin 网关端口。给网关换一个端口，或先改 Devin 端口。") },
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
