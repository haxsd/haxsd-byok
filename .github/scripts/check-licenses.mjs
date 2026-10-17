#!/usr/bin/env node
// 依赖许可门禁：Rust 侧读 `cargo metadata`，前端侧读 `apps/desktop/package-lock.json`。
//
// 它只判定一件事——每份**声明的**许可都落在允许清单里——因此不需要许可证数据库、
// 不需要额外工具，也不需要联网（`cargo metadata` 用的是本机已有的注册表缓存；CI 上
// 依赖是为构建下载过的）。`cargo-deny` 那类工具能给出更完整的裁决（源码扫描、漏洞库、
// 重复版本），但它要在 CI 上多装一个二进制并维护一份配置；这份门禁先覆盖最要紧的
// "混进一个 copyleft/未知许可"，其余写在运行输出的尾部。
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

/// 允许清单：全部是宽松许可，外加 MPL-2.0。
///
/// MPL-2.0 是文件级 copyleft：按原样随产品分发不要求公开本产品的源代码，只要求
/// 被修改过的 MPL 文件仍然开源。构建过程不修改任何依赖，所以它和宽松许可一样安全；
/// 真正需要拦的是 GPL/AGPL/SSPL 这类会传染到整个产品的许可。
///
/// `CDLA-Permissive-2.0` 是数据许可（webpki 的根证书数据），`Unicode-3.0` 是
/// Unicode 数据文件，两者都是宽松许可。
///
/// 表里一律写成大写：判定时把表达式里的 id 也转成大写再查表，这样 `Zlib` 与
/// `ZLIB`、`Unlicense` 与 `UNLICENSE` 不会因为大小写不同而被误判成违规。
export const ALLOWED_LICENSES = new Set([
  "0BSD",
  "APACHE-2.0",
  "BSD-2-CLAUSE",
  "BSD-3-CLAUSE",
  "BSL-1.0",
  "CC0-1.0",
  "CDLA-PERMISSIVE-2.0",
  "ISC",
  "MIT",
  "MIT-0",
  "MPL-2.0",
  "UNICODE-3.0",
  "UNLICENSE",
  "ZLIB",
]);

/// 把 Cargo/npm 写下的许可表达式拆成 token。
///
/// 两处历史写法要处理：`A/B` 按 Cargo 的约定表示"或"；`WITH <exception>` 只是额外
/// 放宽权限（例如 `Apache-2.0 WITH LLVM-exception`），判定按基础许可走。
export function tokenize(expression) {
  return expression
    .replace(/\bWITH\s+[A-Za-z0-9.-]+/gi, " ")
    .replace(/\//g, " OR ")
    .replace(/\(/g, " ( ")
    .replace(/\)/g, " ) ")
    .split(/\s+/)
    .filter(Boolean);
}

function parse(tokens) {
  let index = 0;
  const peek = () => tokens[index];

  function parseOr() {
    const items = [parseAnd()];
    while (peek()?.toUpperCase() === "OR") {
      index += 1;
      items.push(parseAnd());
    }
    return items.length === 1 ? items[0] : { kind: "or", items };
  }

  function parseAnd() {
    const items = [parseTerm()];
    while (peek()?.toUpperCase() === "AND") {
      index += 1;
      items.push(parseTerm());
    }
    return items.length === 1 ? items[0] : { kind: "and", items };
  }

  function parseTerm() {
    const token = peek();
    if (token === undefined) throw new Error("license expression ends early");
    index += 1;
    if (token === "(") {
      const inner = parseOr();
      if (peek() !== ")") throw new Error("license expression has an unclosed group");
      index += 1;
      return inner;
    }
    if (token === ")") throw new Error("license expression has an unexpected )");
    if (token.toUpperCase() === "AND" || token.toUpperCase() === "OR") {
      throw new Error(`license expression has a dangling ${token}`);
    }
    return { kind: "id", id: token };
  }

  const node = parseOr();
  if (index !== tokens.length) {
    throw new Error(`license expression has trailing tokens: ${tokens.slice(index).join(" ")}`);
  }
  return node;
}

function satisfies(node, allowed) {
  if (node.kind === "id") return allowed.has(node.id.toUpperCase());
  if (node.kind === "or") return node.items.some((item) => satisfies(item, allowed));
  return node.items.every((item) => satisfies(item, allowed));
}

function identifiers(node) {
  if (node.kind === "id") return [node.id];
  return node.items.flatMap(identifiers);
}

/// 判定一份许可表达式：允许则 `allowed` 为真，否则 `unmet` 列出不在清单里的许可。
export function licenseVerdict(expression, allowed = ALLOWED_LICENSES) {
  let node;
  try {
    node = parse(tokenize(expression));
  } catch (error) {
    return { allowed: false, unmet: [], reason: error.message };
  }
  if (satisfies(node, allowed)) return { allowed: true, unmet: [] };
  const unmet = identifiers(node).filter((id) => !allowed.has(id.toUpperCase()));
  return {
    allowed: false,
    unmet: [...new Set(unmet)],
    reason: unmet.length === 0 ? "no branch of the expression is allowed" : undefined,
  };
}

/// Rust 侧：工作区自身不算第三方；其余包必须声明许可，且声明必须被允许。
export function checkCargoPackages(metadata, allowed = ALLOWED_LICENSES) {
  const workspace = new Set(metadata.workspace_members ?? []);
  const violations = [];
  const unverifiable = [];
  let checked = 0;
  for (const pkg of metadata.packages ?? []) {
    if (workspace.has(pkg.id)) continue;
    if (pkg.source === null || pkg.source === undefined) continue;
    checked += 1;
    const name = `${pkg.name} ${pkg.version}`;
    if (!pkg.license) {
      unverifiable.push(`${name}${pkg.license_file ? ` (license-file ${pkg.license_file})` : ""}`);
      continue;
    }
    const verdict = licenseVerdict(pkg.license, allowed);
    if (!verdict.allowed) {
      violations.push(`${name}: ${pkg.license}${verdict.unmet.length ? ` (unmet: ${verdict.unmet.join(", ")})` : ` (${verdict.reason})`}`);
    }
  }
  return { checked, violations, unverifiable };
}

/// 前端侧：`package-lock.json` 的 `packages` 映射。
///
/// 没有 `license` 字段的条目单列成"无法判定"而不是直接失败：那是 npm 自己没写进锁文件
/// 的元数据缺口，不是本仓库能改的依赖选择，把它当失败会让门禁永远红。声明的许可一律判定。
export function checkLockfile(lock, allowed = ALLOWED_LICENSES) {
  const violations = [];
  const unverifiable = [];
  let checked = 0;
  for (const [path, entry] of Object.entries(lock.packages ?? {})) {
    if (!path || entry.link) continue;
    checked += 1;
    if (!entry.license) {
      unverifiable.push(path);
      continue;
    }
    const verdict = licenseVerdict(entry.license, allowed);
    if (!verdict.allowed) {
      violations.push(`${path}: ${entry.license}${verdict.unmet.length ? ` (unmet: ${verdict.unmet.join(", ")})` : ` (${verdict.reason})`}`);
    }
  }
  return { checked, violations, unverifiable };
}

function cargoMetadata() {
  const stdout = execFileSync(
    "cargo",
    ["metadata", "--locked", "--format-version", "1"],
    { encoding: "utf8", maxBuffer: 256 * 1024 * 1024 },
  );
  return JSON.parse(stdout);
}

function main() {
  const root = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
  const metadataArgument = process.argv.indexOf("--metadata");
  const metadata =
    metadataArgument === -1
      ? cargoMetadata()
      : JSON.parse(readFileSync(process.argv[metadataArgument + 1], "utf8"));
  const lockfile = JSON.parse(
    readFileSync(resolve(root, "apps/desktop/package-lock.json"), "utf8"),
  );

  const cargo = checkCargoPackages(metadata);
  const frontend = checkLockfile(lockfile);
  console.log(
    `cargo: ${cargo.checked} crates checked, ${cargo.violations.length} violations, ${cargo.unverifiable.length} without a declared license`,
  );
  console.log(
    `npm: ${frontend.checked} packages checked, ${frontend.violations.length} violations, ${frontend.unverifiable.length} without a declared license`,
  );
  for (const entry of cargo.violations) console.error(`  cargo violation: ${entry}`);
  for (const entry of frontend.violations) console.error(`  npm violation: ${entry}`);
  for (const entry of [...cargo.unverifiable, ...frontend.unverifiable]) {
    console.warn(`  license not recorded upstream: ${entry}`);
  }
  const violations = cargo.violations.length + frontend.violations.length;
  if (violations > 0) {
    console.error(`dependency license check failed with ${violations} violation(s)`);
    process.exitCode = 1;
    return;
  }
  console.log("every declared dependency license is allowed");
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main();
}
