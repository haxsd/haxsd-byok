/** 错误正文进提示条前的长度上限。 */
export const ERROR_BODY_LIMIT = 300;

function clip(text: string): string {
  return text.length > ERROR_BODY_LIMIT ? `${text.slice(0, ERROR_BODY_LIMIT)}…` : text;
}

/**
 * 把一个失败响应变成**一句**能给用户看的话。
 *
 * 以前不管什么正文都直接塞进提示条：反代/网关挡在前面时正文是一整页 HTML，于是界面上
 * 出现一坨标签；服务端的正常错误（`{code,message}`，见 server/src/error.rs）则被整段 JSON
 * 盖住。规则收在这里：
 * - JSON 里有 `message` → 用它（超长截断）。
 * - 纯文本 → 原样显示（服务端有些接口回的就是一句英文原句），超长截断。
 * - 看着像标记语言（HTML/XML）或空正文 → 只报状态码，正文交给控制台。
 */
export function errorBodyMessage(status: number, statusText: string, body: string): string {
  const text = body.trim();
  if (text) {
    try {
      const parsed = JSON.parse(text) as { message?: unknown };
      if (typeof parsed.message === "string" && parsed.message.trim()) return clip(parsed.message.trim());
    } catch {
      if (!text.startsWith("<")) return clip(text);
    }
  }
  const statusLine = `${status} ${statusText}`.trim();
  return statusLine || String(status);
}

/** 该不该把完整正文写进控制台（被丢掉或被截断时留一份现场）。 */
export function errorBodyNeedsConsole(message: string, body: string): boolean {
  return body.trim().length > 0 && message !== body.trim();
}

/** 控制台里保留的正文长度：够看清原因，又不至于把整页 HTML 倒进开发者工具。 */
export const CONSOLE_BODY_LIMIT = 4_000;

export function clipConsoleBody(body: string): string {
  return body.length > CONSOLE_BODY_LIMIT ? `${body.slice(0, CONSOLE_BODY_LIMIT)}…（共 ${body.length} 字符）` : body;
}
