import type { PaletteCommand } from "./commandPaletteCommands";

/** 调用记录的命令 id 前缀：默认列表不列它们（可能有几百条）。 */
export const CALL_COMMAND_PREFIX = "call-";

/** 空查询时最多给几条。 */
export const EMPTY_QUERY_LIMIT = 12;

/** 非空查询时最多给几条。 */
export const QUERY_LIMIT = 14;

/**
 * 空查询的默认列表：**按组给名额**，而不是整体切前 N 条。
 *
 * 整体 slice 的话，「页面」7 条 + 「接入模块」3 条就把 12 条名额吃满了，后面的动作和模型
 * 一条都进不来——注释写着「只给页面、动作和模型」，实际永远列不到模型。这里每组先分
 * `floor(限额/组数)` 条，剩下的名额再按组顺序补；组的先后顺序保持连续（渲染按相邻项合并
 * 分组标题，交错会把同一个标题印好几次）。
 */
export function emptyQueryMatches(commands: PaletteCommand[], limit = EMPTY_QUERY_LIMIT): PaletteCommand[] {
  const groups: PaletteCommand[][] = [];
  const indexOfGroup = new Map<string, number>();
  for (const command of commands) {
    if (command.id.startsWith(CALL_COMMAND_PREFIX)) continue;
    const known = indexOfGroup.get(command.group);
    if (known === undefined) {
      indexOfGroup.set(command.group, groups.length);
      groups.push([command]);
    } else {
      groups[known].push(command);
    }
  }
  if (!groups.length) return [];

  const quota = Math.max(1, Math.floor(limit / groups.length));
  // 名额先算清楚再输出：补齐剩余名额要按轮次轮流分，但输出必须按组连续
  //（渲染按相邻项合并分组标题，交错会把同一个标题印好几次）。
  const counts = groups.map((group) => Math.min(quota, group.length));
  let remaining = limit - counts.reduce((total, count) => total + count, 0);
  while (remaining > 0) {
    let placed = false;
    for (let index = 0; index < groups.length && remaining > 0; index += 1) {
      if (counts[index] < groups[index].length) {
        counts[index] += 1;
        remaining -= 1;
        placed = true;
      }
    }
    if (!placed) break;
  }
  const rows: PaletteCommand[] = [];
  groups.forEach((group, index) => rows.push(...group.slice(0, counts[index])));
  return rows;
}

/**
 * 命中越靠前分越高，全部字符按顺序出现也算命中（"dvn" 能命中 "Devin"）。
 * 返回 0 表示不命中。
 */
export function matchScore(command: PaletteCommand, query: string): number {
  const label = String(command.label ?? "").toLowerCase();
  const haystack = `${label} ${String(command.hint ?? "").toLowerCase()} ${String(command.keywords ?? "").toLowerCase()}`;
  if (label.startsWith(query)) return 100;
  if (label.includes(query)) return 80;
  if (haystack.includes(query)) return 60;
  let position = 0;
  for (const character of query) {
    position = label.indexOf(character, position);
    if (position < 0) return 0;
    position += 1;
  }
  return 20;
}

/** 面板当前该显示哪些命令。 */
export function paletteMatches(commands: PaletteCommand[], query: string): PaletteCommand[] {
  const trimmed = query.trim().toLowerCase();
  if (!trimmed) return emptyQueryMatches(commands);
  return commands
    .map((command) => ({ command, score: matchScore(command, trimmed) }))
    .filter((entry) => entry.score > 0)
    .sort((left, right) => right.score - left.score)
    .slice(0, QUERY_LIMIT)
    .map((entry) => entry.command);
}
