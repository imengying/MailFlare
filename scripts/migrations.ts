/** 为曾执行过 0002_push_status.sql 的线上数据库准备迁移副本。 */
import { cp, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

interface Column {
  name: string;
  type: string;
  notnull: number;
  dflt_value: unknown;
  pk: number;
}

interface MigrationOptions {
  configPath?: string;
  local?: boolean;
  persistTo?: string;
}

function run(args: string[], capture = false): string {
  const result = spawnSync("bun", ["x", "wrangler", ...args], {
    encoding: "utf8",
    stdio: capture ? ["ignore", "pipe", "pipe"] : ["ignore", "inherit", "inherit"],
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    if (capture) console.error(`${result.stdout ?? ""}${result.stderr ?? ""}`.trim());
    throw new Error("数据库迁移检查或执行失败，Worker 未发布");
  }
  // --json 的 stdout 单独解析，不能混入 stderr 中的代理/版本提示。
  return result.stdout ?? "";
}

export async function applyMigrations(options: MigrationOptions = {}): Promise<void> {
  const configPath = resolve(options.configPath ?? "wrangler.toml");
  const config = Bun.TOML.parse(await readFile(configPath, "utf8")) as {
    name: string;
    account_id?: string;
    d1_databases: Array<{
      binding: string;
      database_name: string;
      database_id?: string;
      migrations_dir?: string;
      migrations_table?: string;
    }>;
  };
  const database = config.d1_databases.find(db => db.binding === "DB");
  if (!database) throw new Error("wrangler.toml 缺少 DB 绑定");
  const target = [options.local ? "--local" : "--remote"];
  if (options.persistTo) target.push("--persist-to", resolve(options.persistTo));
  const result = JSON.parse(run([
    "d1", "execute", "DB", "--config", configPath, ...target,
    "--command", "PRAGMA table_info(emails)", "--json",
  ], true)) as Array<{ success: boolean; results: Column[] }>;
  if (result.length !== 1 || result[0]?.success !== true || !Array.isArray(result[0].results)) {
    throw new Error("无法确认 emails 表结构，停止迁移");
  }
  const pushStatus = result[0].results.find(column => column.name.toLowerCase() === "push_status");
  if (!pushStatus) {
    run(["d1", "migrations", "apply", "DB", "--config", configPath, ...target]);
    return;
  }
  if (pushStatus.type.toUpperCase() !== "TEXT" || pushStatus.notnull !== 0
    || pushStatus.dflt_value !== null || pushStatus.pk !== 0) {
    throw new Error("已有 push_status 列与预期的可空 TEXT 不一致，停止迁移");
  }

  const source = resolve(dirname(configPath), database.migrations_dir ?? "migrations");
  const filename = "0002_reliability.sql";
  const sql = await readFile(join(source, filename), "utf8");
  const addition = /^ALTER TABLE emails ADD COLUMN push_status TEXT;\s*$/gm;
  if ([...sql.matchAll(addition)].length !== 1) {
    throw new Error("0002 中 push_status 语句与预期不符，停止迁移");
  }
  const temporary = await mkdtemp(join(tmpdir(), "mailflare-migrations-"));
  try {
    const migrationsDir = join(temporary, "migrations");
    await cp(source, migrationsDir, { recursive: true });
    await writeFile(join(migrationsDir, filename), sql.replace(addition, "-- push_status 已由历史迁移添加，保留现有值。\n"));
    const temporaryConfig = join(temporary, "wrangler.json");
    await writeFile(temporaryConfig, JSON.stringify({
      name: config.name,
      ...(config.account_id ? { account_id: config.account_id } : {}),
      d1_databases: [{ ...database, migrations_dir: migrationsDir }],
    }));
    console.log("    已有 push_status 列，保留其数据；其余迁移照常执行");
    // 仍由 Wrangler 完整执行并记账；绝不手工将未执行的迁移标为成功。
    run(["d1", "migrations", "apply", "DB", "--config", temporaryConfig, ...target]);
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
}
