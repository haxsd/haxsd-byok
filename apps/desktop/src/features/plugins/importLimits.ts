/**
 * 批量导入的本地准入检查。
 *
 * 导入要把每个文件整个读成字符串、拼进请求体：没有上限时，用户选中一个几百兆的文件就会
 * 先把界面进程的内存吃掉，再被服务端的请求体限制拒绝——而且失败发生在读完文件之后，用户
 * 等半天只等到一句"太大"。所以在读文件之前先按体积判断，并把大批量拆成多次请求。
 */

/** 单个文件上限。导入的是账号/规则这类文本，8 MiB 已经远超正常体量。 */
export const MAX_IMPORT_FILE_BYTES = 8 * 1024 * 1024;
/** 一次导入的总上限：内存占用与请求体大小都按这个量级封顶。 */
export const MAX_IMPORT_TOTAL_BYTES = 32 * 1024 * 1024;
/** 每次请求最多带几个文件，避免单次请求体过大。 */
export const IMPORT_FILES_PER_REQUEST = 20;

export type ImportSizeProblem =
  | { kind: "file-too-large"; name: string }
  | { kind: "total-too-large" };

/** `null` 表示可以开始读文件。 */
export function importSizeProblem(files: ReadonlyArray<{ name: string; size: number }>): ImportSizeProblem | null {
  const oversized = files.find((file) => file.size > MAX_IMPORT_FILE_BYTES);
  if (oversized) return { kind: "file-too-large", name: oversized.name };
  const total = files.reduce((sum, file) => sum + file.size, 0);
  return total > MAX_IMPORT_TOTAL_BYTES ? { kind: "total-too-large" } : null;
}

/** 按固定条数切片，保持原顺序。 */
export function importBatches<T>(files: readonly T[]): T[][] {
  const batches: T[][] = [];
  for (let index = 0; index < files.length; index += IMPORT_FILES_PER_REQUEST) {
    batches.push(files.slice(index, index + IMPORT_FILES_PER_REQUEST));
  }
  return batches;
}
