import type { DevinModelBinding, DevinModelChoice, DevinRoute } from "../../shared/api";

const fallbackModelUid = "swe-1-6-slow";

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
