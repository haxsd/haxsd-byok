/**
 * 调用详情里「响应流」的预览截断。
 *
 * 一次真实调用能产生几 MB 的响应正文；一次性把它交给 Monaco，打开详情要等两三秒，
 * 窗口像卡死。预览只取前一段，剩下的由用户按需展开——判断与拼接放在这里，便于测试。
 */

/** 预览默认保留多少字节；超出就截断。一次几百 KB 的正文 Monaco 渲染在几十毫秒内。 */
export const RESPONSE_PREVIEW_BYTES = 256 * 1024;

export interface ResponsePreview {
  /** 实际渲染的正文（可能只是前缀）。 */
  text: string;
  /** 已展示部分的字节数（按服务端记录的分块字节计）。 */
  shownBytes: number;
  /** 整段响应的字节数。 */
  totalBytes: number;
  totalChunks: number;
  /** 是否为截断预览。 */
  truncated: boolean;
}

interface Chunk {
  data?: unknown;
  byte_count?: unknown;
}

const byteCount = (chunk: Chunk) => (typeof chunk.byte_count === "number" && Number.isFinite(chunk.byte_count)
  ? chunk.byte_count
  : typeof chunk.data === "string" ? chunk.data.length : 0);

/**
 * 按字节预算截断响应流。
 *
 * `showAll` 为真时原样返回整段（用户点过「显示全部」）。分块顺序就是服务端收到的顺序，
 * 这里不重排、不改写内容；只决定停在哪一块。
 */
export function responsePreview(
  chunks: readonly Chunk[],
  options: { limitBytes?: number; showAll?: boolean } = {},
): ResponsePreview {
  const limitBytes = options.limitBytes ?? RESPONSE_PREVIEW_BYTES;
  const totalChunks = chunks.length;
  const totalBytes = chunks.reduce((total, chunk) => total + byteCount(chunk), 0);

  if (options.showAll) {
    return {
      text: chunks.map((chunk) => (typeof chunk.data === "string" ? chunk.data : "")).join(""),
      shownBytes: totalBytes,
      totalBytes,
      totalChunks,
      truncated: false,
    };
  }

  let shownBytes = 0;
  const parts: string[] = [];
  for (const chunk of chunks) {
    // 至少渲染一块：第一块就超过预算时也要给用户看到开头，而不是空白。
    if (shownBytes >= limitBytes && parts.length > 0) break;
    parts.push(typeof chunk.data === "string" ? chunk.data : "");
    shownBytes += byteCount(chunk);
  }

  return {
    text: parts.join(""),
    shownBytes,
    totalBytes,
    totalChunks,
    truncated: parts.length < totalChunks,
  };
}
