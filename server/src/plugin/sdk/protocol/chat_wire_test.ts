import { chatFinish, mergeArguments, mergeIdentifier } from "./chat_wire.ts";
// 与 Rust 侧 `provider/wire/tests.rs` 读同一份表；改行为时先改表。
import cases from "../../../provider/wire/cases.json" with { type: "json" };

function assert(condition: unknown, message = "assertion failed"): asserts condition {
  if (!condition) throw new Error(message);
}

Deno.test("the shared Chat wire cases hold in TypeScript", () => {
  for (const test of cases.merge_identifier) {
    assert(
      mergeIdentifier(test.target, test.fragment) === test.expect,
      `identifier ${JSON.stringify(test.fragment)}`,
    );
  }
  for (const test of cases.merge_arguments) {
    assert(
      mergeArguments(test.target, test.fragment) === test.expect,
      `arguments ${JSON.stringify(test.fragment)}`,
    );
  }
  for (const test of cases.chat_finish) {
    assert(chatFinish(test.reason) === test.expect, `finish ${JSON.stringify(test.reason)}`);
  }
});
