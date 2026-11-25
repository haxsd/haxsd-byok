import assert from "node:assert/strict";
import test from "node:test";
import { parseTimeInput } from "../src/shared/utils/parseTimeInput.ts";

const NOW = new Date(Date.UTC(2026, 10, 25, 12, 0, 0));

test("parseTimeInput supports Russian keywords and relative units", () => {
  assert.equal(parseTimeInput("сейчас", NOW), NOW.getTime());

  const today = new Date(NOW);
  today.setHours(0, 0, 0, 0);
  assert.equal(parseTimeInput("сегодня", NOW), today.getTime());

  const yesterday = new Date(NOW);
  yesterday.setHours(0, 0, 0, 0);
  yesterday.setDate(yesterday.getDate() - 1);
  assert.equal(parseTimeInput("вчера", NOW), yesterday.getTime());

  assert.equal(parseTimeInput("15 мин назад", NOW), NOW.getTime() - 15 * 60_000);
  assert.equal(parseTimeInput("2 часа назад", NOW), NOW.getTime() - 2 * 3_600_000);
  assert.equal(parseTimeInput("3 дня назад", NOW), NOW.getTime() - 3 * 86_400_000);
  assert.equal(parseTimeInput("1 неделя назад", NOW), NOW.getTime() - 7 * 86_400_000);
});
