import assert from "node:assert/strict";
import test from "node:test";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const binding = (overrides = {}) => ({
  model_uid: "swe-1-6-slow",
  model_hash: "hash-1",
  display_name: "Example",
  context_window_tokens: null,
  enabled: true,
  kind: "standard",
  routes: [],
  active_route_id: null,
  ...overrides,
});

const route = (overrides = {}) => ({
  route_id: "route-1",
  model_hash: "hash-2",
  label: "Fallback",
  enabled: true,
  ...overrides,
});

test("a new model uses a free UID and refuses to duplicate an occupied one", async () => {
  const { nextModelUid } = await loader.load("/src/features/devin/devinBindingEdits.ts");
  const choices = [
    { uid: "swe-1-6-slow", name: "SWE-1.6 Slow" },
    { uid: "claude-sonnet", name: "Claude Sonnet" },
  ];

  assert.equal(nextModelUid([], choices), "swe-1-6-slow");
  assert.equal(nextModelUid([binding()], choices), "claude-sonnet");
  assert.equal(
    nextModelUid([binding(), binding({ model_uid: "claude-sonnet" })], choices),
    null,
    "all available Devin UIDs are already mapped",
  );
});

test("the first fallback route does not become the active route", async () => {
  const { appendRouteForBinding } = await loader.load("/src/features/devin/devinBindingEdits.ts");

  assert.deepEqual(
    appendRouteForBinding(binding(), route()),
    { routes: [route()], active_route_id: null },
  );

  const existing = binding({ routes: [route()], active_route_id: null });
  assert.deepEqual(
    appendRouteForBinding(existing, route({ route_id: "route-2" })),
    { routes: [route(), route({ route_id: "route-2" })], active_route_id: null },
  );

  const active = binding({ routes: [route()], active_route_id: "route-1" });
  assert.equal(appendRouteForBinding(active, route({ route_id: "route-2" })).active_route_id, "route-1");
});

test("editing routes keeps only the explicitly active route", async () => {
  const { keepActiveRouteId } = await loader.load("/src/features/devin/devinBindingEdits.ts");
  const routes = [route(), route({ route_id: "route-2", enabled: false })];

  assert.equal(keepActiveRouteId(routes, "route-1"), "route-1");
  assert.equal(keepActiveRouteId(routes, null), null, "no explicit choice means the primary model stays in use");
  assert.equal(keepActiveRouteId(routes, "route-2"), null, "a disabled route cannot become current");
  assert.equal(keepActiveRouteId(routes, "missing"), null, "removing the current route must not promote a fallback");
  assert.equal(keepActiveRouteId([], "route-1"), null);
});

test("the effective model follows the enabled active route", async () => {
  const { effectiveBindingModelHash } = await loader.load("/src/features/devin/devinBindingEdits.ts");

  assert.equal(effectiveBindingModelHash(binding()), "hash-1");
  assert.equal(
    effectiveBindingModelHash(binding({
      routes: [route({ route_id: "route-2", model_hash: "hash-2" })],
      active_route_id: "route-2",
    })),
    "hash-2",
  );
  assert.equal(
    effectiveBindingModelHash(binding({
      routes: [route({ route_id: "route-2", model_hash: "hash-2", enabled: false })],
      active_route_id: "route-2",
    })),
    "hash-1",
  );
});
