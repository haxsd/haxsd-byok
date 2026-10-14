// Behavior-test harness for the desktop frontend.
//
// The application source is TypeScript with extensionless imports, SCSS modules,
// `import.meta.env`, and a build-time `t()` i18n import. Node cannot load it
// directly, so tests load the *real* modules through the project's own Vite
// pipeline (no bundling step, no copied sources) and replace only the two
// boundaries the browser would provide: `fetch` (HTTP) and the Tauri `window`
// (native calls). Assertions run against the real store/api/lifecycle code.

import path from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

export const desktopRoot = path.resolve(fileURLToPath(new URL("..", import.meta.url)));

/** 本地管理 API 的前缀，和 src/shared/api.ts 里的 API_ROOT 保持一致。 */
export const API_ROOT = "/__byok-api__/api";

/**
 * Start a Vite dev server used purely as a module loader for the tests.
 *
 * `configFile: false` keeps the production config (i18n scan plugin, code
 * inspector, build chunking) out of the test loop; TypeScript transformation and
 * extensionless resolution still come from Vite itself.
 */
export async function startModuleLoader() {
  const server = await createServer({
    configFile: false,
    root: desktopRoot,
    appType: "custom",
    logLevel: "error",
    // No watch socket: every test file boots its own loader, and a shared HMR port
    // would collide between them (and keep the process alive).
    server: { middlewareMode: true, hmr: false, ws: false },
  });
  return {
    /** Load one application module the way the app would import it. */
    load: (modulePath) => server.ssrLoadModule(modulePath),
    /** Drop the module graph so the next load re-executes module init code. */
    invalidateAll: () => server.moduleGraph.invalidateAll(),
    close: () => server.close(),
  };
}

/** Minimal DOM surface the loaded modules touch at import time. */
export function installBrowserDoubles({ origin = "http://127.0.0.1:3100" } = {}) {
  const storage = new Map();
  const location = new URL("/__byok-api__/", origin);
  const tauriInvocations = [];
  const localStorage = {
    getItem: (key) => (storage.has(key) ? storage.get(key) : null),
    setItem: (key, value) => storage.set(key, String(value)),
    removeItem: (key) => storage.delete(key),
  };
  globalThis.window = {
    location,
    localStorage,
    __TAURI_INTERNALS__: {
      invoke: async (command, args) => {
        tauriInvocations.push({ command, args });
        return undefined;
      },
    },
  };
  // Application modules call these as bare globals (the browser makes them
  // window properties; Node does not).
  globalThis.localStorage = localStorage;
  globalThis.document = { documentElement: { dataset: {} } };
  // The i18n plugin is not part of the loader config, so `t()` resolves from the
  // global scope. Interpolate the same `{name}` placeholders the catalogs use.
  globalThis.t = (message, params) => {
    if (typeof message !== "string" || !params) return message;
    return message.replace(/\{([A-Za-z_][A-Za-z0-9_]*)\}/g, (match, key) =>
      Object.prototype.hasOwnProperty.call(params, key) ? String(params[key]) : match);
  };
  return { storage, location, tauriInvocations };
}

/**
 * A controllable stand-in for the local management server.
 *
 * Routes are keyed by `"METHOD /path"`; a handler returns `{ status?, body? }` or
 * a promise for it, which lets a test hold one response open while another
 * request overtakes it.
 */
export class FakeBackend {
  constructor() {
    this.routes = new Map();
    /** Every request the frontend sent, in order. */
    this.requests = [];
  }

  route(method, path, handler) {
    this.routes.set(`${method} ${path}`, handler);
    return this;
  }

  install() {
    globalThis.fetch = (input, init = {}) => this.#handle(input, init);
  }

  /** Requests matching a method and path, for boundary assertions. */
  requestsFor(method, path) {
    return this.requests.filter((request) => request.method === method && request.path === path);
  }

  async #handle(input, init) {
    const url = input instanceof URL ? input : new URL(String(input), "http://127.0.0.1");
    const method = (init.method ?? "GET").toUpperCase();
    const record = {
      method,
      path: url.pathname,
      search: url.search,
      body: typeof init.body === "string" ? JSON.parse(init.body) : null,
      signal: init.signal ?? null,
    };
    this.requests.push(record);
    const handler = this.routes.get(`${method} ${url.pathname}`);
    if (!handler) throw new TypeError(`FakeBackend has no route for ${method} ${url.pathname}`);
    const reply = await handler(record);
    const body = reply?.body;
    return new Response(body === undefined ? null : JSON.stringify(body), {
      status: reply?.status ?? (body === undefined ? 204 : 200),
      headers: body === undefined ? undefined : { "content-type": "application/json" },
    });
  }
}

/** A promise plus its resolver, for ordering two racing responses by hand. */
export function deferred() {
  let resolve;
  const promise = new Promise((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}
