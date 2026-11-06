import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import test from "node:test";

const desktop = path.resolve(import.meta.dirname, "..");
const repo = path.resolve(desktop, "../..");

function normalize(route) {
  return route.replace(/\{[^}]+\}/g, "*");
}

/** 控制面源码里注册的 `/__byok-api__/api` 路径。 */
function serverRoutes() {
  const dir = path.join(repo, "server/src/control");
  const files = readdirSync(dir, { recursive: true }).filter((file) => file.endsWith(".rs"));
  const text = files
    .map((file) => readFileSync(path.join(dir, file), "utf8"))
    .join("\n");
  const routes = new Set();
  for (const match of text.matchAll(/"\/__byok-api__\/api([^"]*)"/g)) {
    routes.add(normalize(match[1] || "/"));
  }
  return [...routes].sort();
}

/** Demo 的 fetch 分发实际能命中的路径。 */
function demoPatterns() {
  const text = readFileSync(path.join(desktop, "src/demo/api.ts"), "utf8");
  const patterns = new Set();
  for (const match of text.matchAll(/path === "([^"]+)"/g)) patterns.add(match[1]);
  for (const match of text.matchAll(/path\.startsWith\("([^"]+)"\)/g)) {
    patterns.add(`${match[1]}*`);
  }
  // 模型相关端点用正则匹配；形状变了必须同步这里，否则下面的覆盖断言会失明。
  for (const shape of ["/models/*/duplicate", "/models/*/test/*", "/models/*"]) {
    patterns.add(shape);
  }
  return [...patterns];
}

function matches(pattern, route) {
  if (pattern.endsWith("*") && !pattern.slice(0, -1).includes("*")) {
    return route.startsWith(pattern.slice(0, -1));
  }
  const expected = pattern.split("/").filter(Boolean);
  const actual = route.split("/").filter(Boolean);
  if (expected.length !== actual.length) return false;
  return expected.every((segment, index) => segment === "*" || segment === actual[index]);
}

/**
 * Demo 里没有安装任何插件，插件资源/OAuth 端点不会被调用。这里显式列出，而不是让它们
 * 悄悄 404：服务端新增路由时，测试会失败，必须选择“做进 Demo”或“写明不支持”。
 */
const UNSUPPORTED = new Set([
  "/plugins/*",
  "/plugins/oauth/*/poll",
  "/plugins/*/providers/*/models/enabled",
  "/plugins/*/providers/*/models/sync",
  "/plugins/*/resources/*/*",
  "/plugins/*/resources/*/*/actions/*",
  "/plugins/*/resources/*/*/refresh",
  "/plugins/*/resources/*/add/*/begin",
  "/plugins/*/resources/*/export",
  "/plugins/*/resources/*/import",
]);

test("every control API route is demo-backed or explicitly unsupported", () => {
  const patterns = demoPatterns();
  const missing = serverRoutes().filter(
    (route) => !UNSUPPORTED.has(route) && !patterns.some((pattern) => matches(pattern, route)),
  );
  assert.deepEqual(missing, [], `demo lacks routes: ${missing.join(", ")}`);
});

test("demo shows the real app version and retention window", () => {
  const version = JSON.parse(readFileSync(path.join(desktop, "package.json"), "utf8")).version;
  const demo = readFileSync(path.join(desktop, "src/demo/api.ts"), "utf8");
  assert.match(demo, /version: appVersion/);
  assert.match(demo, /from "\.\.\/\.\.\/package\.json"/);
  assert.ok(
    !demo.includes(`version: "${version}"`),
    "demo must not hardcode the app version",
  );

  const retention = readFileSync(path.join(repo, "server/src/store/retention.rs"), "utf8");
  const milliseconds = /CONVERSATION_RETENTION_MS: i64 =\s*([0-9*\s]+);/.exec(retention)?.[1];
  assert.ok(milliseconds, "conversation retention constant not found");
  const days =
    milliseconds
      .split("*")
      .map((part) => Number(part.trim()))
      .reduce((left, right) => left * right, 1) / 86_400_000;
  const shown = /conversation_retention_days:\s*(\d+)/.exec(demo)?.[1];
  assert.equal(Number(shown), days);
});
