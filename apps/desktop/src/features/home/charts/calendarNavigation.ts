/** 日历是七行网格：左右跨一周，上下跨一天，Home/End 到本周首尾。 */
export function calendarTargetIndex(cells: ReadonlyArray<{ row: number }>, currentIndex: number, key: string) {
  if (cells.length === 0) return 0;
  const current = Math.min(Math.max(0, currentIndex), cells.length - 1);
  const row = cells[current].row;
  switch (key) {
    case "ArrowRight": return Math.min(cells.length - 1, current + 7);
    case "ArrowLeft": return Math.max(0, current - 7);
    case "ArrowDown": return row < 6 ? Math.min(cells.length - 1, current + 1) : current;
    case "ArrowUp": return row > 0 ? Math.max(0, current - 1) : current;
    case "Home": return Math.max(0, current - row);
    case "End": return Math.min(cells.length - 1, current + (6 - row));
    default: return current;
  }
}
