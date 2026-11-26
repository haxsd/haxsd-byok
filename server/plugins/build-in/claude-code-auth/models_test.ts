import type { PluginContext } from "cursor-byok:plugin";
import { CLAUDE_CODE_MODELS, claudeCodeModels } from "./models.ts";

function context(): PluginContext {
  return {
    network: {
      fetch: () => {
        throw new Error("fetch was not expected");
      },
      stream: () => {
        throw new Error("stream was not expected");
      },
    },
    signal: new AbortController().signal,
  };
}

function assert(condition: unknown, message = "assertion failed"): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

Deno.test("Claude catalog exposes the current Fable model without duplicate IDs", async () => {
  const models = await claudeCodeModels.list({ resource: null }, context());
  const ids = models.map((model) => model.id);
  assertEquals(new Set(ids).size, ids.length);
  assert(ids.includes("claude-fable-5-2"), "the current Fable model must be selectable");
  assert(
    models.every((model) => model.maxOutputTokens !== undefined && model.maxOutputTokens > 0),
    "every model needs a positive output limit",
  );
});

Deno.test("Claude catalog keeps the full snapshot list available", () => {
  assert(CLAUDE_CODE_MODELS.length >= 20);
  assert(CLAUDE_CODE_MODELS.some((model) => model.id === "claude-opus-4-5"));
  assert(CLAUDE_CODE_MODELS.some((model) => model.id === "claude-sonnet-4-5-20250929"));
});
