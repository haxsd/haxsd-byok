/**
 * 上游错误消息可能回显请求头或令牌。返回给宿主之前统一抹掉已知凭证，
 * 保证 access/refresh token 不进入日志、错误消息或界面。
 */
export function redactSecrets(message: string, secrets: Array<string | null | undefined>): string {
  let redacted = message;
  for (const secret of secrets) {
    if (secret && secret.length >= 8) redacted = redacted.split(secret).join("[redacted]");
  }
  return redacted;
}

/** 错误正文可能很长；界面只需要可诊断的前缀，不把整个响应体带进资源状态。 */
export function truncate(message: string, limit = 600): string {
  return message.length <= limit ? message : `${message.slice(0, limit)}…`;
}
