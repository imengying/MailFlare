#!/usr/bin/env bun
/**
 * 部署脚本：建库 → 迁移 → 发布。
 *
 * `bun run deploy` 会执行本脚本（Cloudflare 控制台的 Deploy command 也用它）。
 *
 * 三步都能重复执行：
 * 1. D1 不存在就创建（已存在则跳过）
 * 2. 应用 `migrations/` 下未执行的 SQL（D1 自己记录执行过的文件名）
 * 3. `wrangler deploy`
 */

import { spawnSync } from "node:child_process";

/** D1 数据库名，必须与 wrangler.toml 的 database_name 一致。 */
const DATABASE = "mailflare";

const log = (msg: string) => console.log(`\n\x1b[1;34m==> ${msg}\x1b[0m`);

function run(args: string[], capture = false): { ok: boolean; out: string } {
  const result = spawnSync("bun", ["x", "wrangler", ...args], {
    encoding: "utf8",
    // 不 capture 时让 wrangler 直接输出，保留它的进度提示。
    stdio: capture ? ["inherit", "pipe", "pipe"] : "inherit",
  });
  if (result.error) throw result.error;
  return {
    ok: result.status === 0,
    out: `${result.stdout ?? ""}${result.stderr ?? ""}`,
  };
}

/**
 * 确保 D1 存在。
 *
 * `d1 list` 输出的是表格，直接按名字做包含匹配：宁可漏判导致 `create`
 * 报「已存在」再忽略，也不要因为解析表格格式变化而误判。
 */
function ensureDatabase(): void {
  log(`检查 D1 数据库 ${DATABASE}`);
  const listed = run(["d1", "list"], true);
  if (listed.ok && new RegExp(`\\b${DATABASE}\\b`).test(listed.out)) {
    console.log(`    已存在：${DATABASE}`);
    return;
  }

  log(`创建 D1 数据库 ${DATABASE}`);
  const created = run(["d1", "create", DATABASE], true);
  if (created.ok) {
    console.log(`    创建成功`);
    return;
  }
  // 并发部署时可能已被另一个实例创建，视为成功。
  if (/already exists|已经存在/i.test(created.out)) {
    console.log(`    已存在（并发创建），继续`);
    return;
  }
  console.error(created.out.trim());
  throw new Error(`创建 D1 数据库失败`);
}

function main(): void {
  ensureDatabase();

  log("应用数据库迁移");
  if (!run(["d1", "migrations", "apply", "DB", "--remote"]).ok) {
    throw new Error("应用数据库迁移失败");
  }

  log("发布 Worker");
  if (!run(["deploy"]).ok) {
    throw new Error("wrangler deploy 失败");
  }

  log("部署完成");
}

try {
  main();
} catch (error) {
  console.error(`\n部署中断：${error instanceof Error ? error.message : error}`);
  process.exit(1);
}
