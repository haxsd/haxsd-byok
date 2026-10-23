import {
  MAX_CONCURRENT_REQUESTS,
  noteRateLimit,
  parseRetryAfterMs,
  RATE_LIMIT_BASE_MS,
  resetRuntimeForTests,
  runtimeSnapshot,
  singleFlight,
  withAccountSlot,
} from "./runtime.ts";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function assertEquals(actual: unknown, expected: unknown): void {
  const left = JSON.stringify(actual);
  const right = JSON.stringify(expected);
  if (left !== right) throw new Error(`expected ${right}, received ${left}`);
}

function tick(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

Deno.test("only the configured number of requests run concurrently, the rest queue", async () => {
  resetRuntimeForTests();
  const controller = new AbortController();
  const started: number[] = [];
  const gates: Array<() => void> = [];

  const run = (index: number) =>
    withAccountSlot("account", controller.signal, () => {
      started.push(index);
      return new Promise<void>((resolve) => gates.push(resolve));
    });

  const tasks = [run(0), run(1), run(2)];
  await tick();
  assertEquals(started, [0, 1]);
  assertEquals(runtimeSnapshot("account"), {
    activeRequests: MAX_CONCURRENT_REQUESTS,
    queuedRequests: 1,
    cooldownUntilMs: null,
    cooldownKind: null,
    lastRateLimitAtMs: null,
  });

  gates.shift()!();
  await tick();
  assertEquals(started, [0, 1, 2]);
  assertEquals(runtimeSnapshot("account").activeRequests, MAX_CONCURRENT_REQUESTS);

  while (gates.length > 0) gates.shift()!();
  await Promise.all(tasks);
  assertEquals(runtimeSnapshot("account").activeRequests, 0);
  assertEquals(runtimeSnapshot("account").queuedRequests, 0);
});

Deno.test("an aborted request leaves the queue without consuming a slot", async () => {
  resetRuntimeForTests();
  const holding = new AbortController();
  const queued = new AbortController();
  let releaseHeld: Array<() => void> = [];
  const hold = () =>
    withAccountSlot("account", holding.signal, () =>
      new Promise<void>((resolve) => {
        releaseHeld.push(resolve);
      }));
  const held = [hold(), hold()];

  let rejected = false;
  const queuedTask = withAccountSlot("account", queued.signal, () => Promise.resolve()).catch(
    (error: unknown) => {
      rejected = error instanceof Error && error.name === "AbortError";
    },
  );
  await tick();
  assertEquals(runtimeSnapshot("account").queuedRequests, 1);
  queued.abort();
  await queuedTask;
  assert(rejected, "the queued request must reject with AbortError");
  assertEquals(runtimeSnapshot("account").queuedRequests, 0);
  releaseHeld.forEach((release) => release());
  releaseHeld = [];
  await Promise.all(held);
});

Deno.test("retry-after is parsed as integer seconds only", () => {
  assertEquals(parseRetryAfterMs({ "Retry-After": "7" }), 7_000);
  assertEquals(parseRetryAfterMs({ "retry-after": " 12 " }), 12_000);
  assertEquals(parseRetryAfterMs({ "Retry-After": "0" }), null);
  assertEquals(parseRetryAfterMs({ "Retry-After": "1.5" }), null);
  assertEquals(parseRetryAfterMs({ "Retry-After": "Wed, 21 Oct 2015 07:28:00 GMT" }), null);
  assertEquals(parseRetryAfterMs({}), null);
});

Deno.test("429 cooling follows retry-after strictly and falls back to jittered backoff", () => {
  resetRuntimeForTests();
  const now = 1_000_000;
  assertEquals(noteRateLimit("retry-after", 90_000, now), now + 90_000);
  assertEquals(runtimeSnapshot("retry-after").cooldownUntilMs, now + 90_000);
  assertEquals(runtimeSnapshot("retry-after").cooldownKind, "rate-limit");

  const until = noteRateLimit("backoff", null, now);
  assert(
    until >= now + RATE_LIMIT_BASE_MS * 0.5 && until <= now + RATE_LIMIT_BASE_MS * 1.5,
    `unexpected fallback backoff: ${until - now}ms`,
  );
});

Deno.test("single-flight collapses concurrent refreshes and clears afterwards", async () => {
  resetRuntimeForTests();
  let calls = 0;
  let resolve: ((value: string) => void) | null = null;
  const task = () =>
    singleFlight("refresh", () => {
      calls += 1;
      return new Promise<string>((finish) => {
        resolve = finish;
      });
    });

  const first = task();
  const second = task();
  await tick();
  assertEquals(calls, 1);
  resolve!("tokens");
  assertEquals(await first, "tokens");
  assertEquals(await second, "tokens");

  const third = singleFlight("refresh", () => Promise.resolve("again"));
  assertEquals(await third, "again");
  assertEquals(calls, 1);
  assert(calls === 1, "a completed flight must not be reused");
});
