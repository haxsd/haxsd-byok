import type { DevinModelBinding, DevinModelChoice, DevinRoute, Model } from "../../shared/api";

const fallbackModelUid = "swe-1-6-slow";
const nativeModelUidPrefix = "haxsd-byok:";

export function keepActiveRouteId(
  routes: DevinRoute[],
  preferred: string | null,
): string | null {
  if (!preferred) return null;
  return routes.some((route) => route.route_id === preferred && route.enabled)
    ? preferred
    : null;
}

export function effectiveBindingModelHash(binding: DevinModelBinding): string {
  if (!binding.active_route_id) return binding.model_hash;
  const route = binding.routes.find(
    (candidate) => candidate.route_id === binding.active_route_id && candidate.enabled,
  );
  return route?.model_hash ?? binding.model_hash;
}

export function nextModelUid(
  bindings: DevinModelBinding[],
  choices: DevinModelChoice[],
): string | null {
  const used = new Set(bindings.map((binding) => binding.model_uid.trim()));
  if (!choices.length) return used.has(fallbackModelUid) ? null : fallbackModelUid;
  return choices.find((choice) => !used.has(choice.uid.trim()))?.uid ?? null;
}

export function isNativeModelUid(uid: string): boolean {
  return uid.trim().startsWith(nativeModelUidPrefix);
}

export function nextNativeModelUid(
  bindings: DevinModelBinding[],
  modelHash: string,
): string {
  const used = new Set(bindings.map((binding) => binding.model_uid.trim()));
  const base = `${nativeModelUidPrefix}${modelHash.trim() || "model"}`;
  if (!used.has(base)) return base;
  let suffix = 2;
  while (used.has(`${base}:${suffix}`)) suffix += 1;
  return `${base}:${suffix}`;
}

/**
 * Add a model from the user library as a native Devin picker entry. Reuse each
 * model and display name only once so the picker's label-to-UID mapping stays
 * unambiguous.
 */
export function nativeBindingForNextModel(
  bindings: DevinModelBinding[],
  models: Model[],
): DevinModelBinding | null {
  const nativeBindings = bindings.filter((binding) => isNativeModelUid(binding.model_uid));
  const usedHashes = new Set(nativeBindings.map(effectiveBindingModelHash));
  const usedNames = new Set(
    nativeBindings
      .map((binding) => binding.display_name.trim())
      .filter(Boolean),
  );
  const model = models.find((candidate) => (
    !usedHashes.has(candidate.model_hash)
    && !usedNames.has(candidate.display_name.trim())
  ));
  if (!model) return null;
  return {
    model_uid: nextNativeModelUid(bindings, model.model_hash),
    model_hash: model.model_hash,
    display_name: model.display_name,
    context_window_tokens: model.context_window_tokens,
    enabled: true,
    routes: [],
    active_route_id: null,
  };
}

/** Adding a fallback must not silently replace the primary model. */
export function appendRouteForBinding(
  binding: DevinModelBinding,
  route: DevinRoute,
): Pick<DevinModelBinding, "routes" | "active_route_id"> {
  const routes = [...binding.routes, route];
  return {
    routes,
    active_route_id: keepActiveRouteId(routes, binding.active_route_id),
  };
}
