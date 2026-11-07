/** 新库初始化；已有数据库只检查结构，升级 SQL 由管理员手动执行。 */
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { spawnSync } from "node:child_process";

interface DatabaseOptions {
  configPath?: string;
  local?: boolean;
  persistTo?: string;
}

type Row = Record<string, unknown>;
const requiredColumns = {
  configs: "key value updated_at",
  emails: "id message_id sender recipient subject text_body html_body attachments is_read created_at push_status raw_key storage_status deleted_at otp_code",
  inbound_jobs: "id envelope_from envelope_to received_at attempts available_at lease_until last_error",
  notification_jobs: "email_id channel status attempts available_at expires_at lease_until",
  login_attempts: "source attempts reset_at",
};
const tables = Object.keys(requiredColumns) as Array<keyof typeof requiredColumns>;

function run(args: string[], capture = false): string {
  const result = spawnSync("bun", ["x", "wrangler", ...args], {
    encoding: "utf8",
    stdio: capture ? ["ignore", "pipe", "pipe"] : ["ignore", "inherit", "inherit"],
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    if (capture) console.error(`${result.stdout ?? ""}${result.stderr ?? ""}`.trim());
    throw new Error("数据库初始化或结构检查失败，Worker 未发布");
  }
  // --json 的 stdout 单独解析，不能混入 stderr 中的代理/版本提示。
  return result.stdout ?? "";
}

export async function prepareDatabase(options: DatabaseOptions = {}): Promise<void> {
  const configPath = resolve(options.configPath ?? "wrangler.toml");
  const config = Bun.TOML.parse(await readFile(configPath, "utf8")) as {
    d1_databases: Array<{ binding: string; migrations_table?: string }>;
  };
  const database = config.d1_databases.find(db => db.binding === "DB");
  if (!database) throw new Error("wrangler.toml 缺少 DB 绑定");
  const target = ["DB", "--config", configPath, options.local ? "--local" : "--remote"];
  if (options.persistTo) target.push("--persist-to", resolve(options.persistTo));
  // --file 在远端走数据库导入且不返回查询行；只读检查必须使用 --command。
  const inspect = (): Row[][] => {
    const statements = [
      "SELECT name FROM sqlite_master WHERE type = 'table'",
      ...tables.map(table => `PRAGMA table_info(${table})`),
      "PRAGMA index_list(emails)",
      "PRAGMA index_info(idx_emails_message_recipient)",
    ];
    const results = JSON.parse(run(["d1", "execute", ...target, "--command", statements.join(";\n") + ";", "--json"], true)) as Array<{ success: boolean; results: Row[] }>;
    if (results.length !== statements.length || results.some(result => result.success !== true || !Array.isArray(result.results))) {
      throw new Error("无法确认数据库结构，Worker 未发布");
    }
    return results.map(result => result.results);
  };
  let schema = inspect();
  const ledger = database.migrations_table ?? "d1_migrations";
  const existingTables = schema[0]!.filter(row => {
    const name = String(row["name"]);
    return name !== ledger && !name.startsWith("sqlite_") && !name.startsWith("_cf_");
  });
  if (existingTables.length === 0) {
    console.log("    空数据库，使用 0001 初始化完整表结构");
    run(["d1", "migrations", "apply", ...target]);
    schema = inspect();
  }
  const missing = tables.flatMap((table, index) => {
    const columns = new Set(schema[index + 1]!.map(column => column["name"]));
    return requiredColumns[table].split(" ").filter(name => !columns.has(name)).map(name => `${table}.${name}`);
  });
  if (missing.length > 0) {
    const hint = missing.length === 1 && missing[0] === "emails.otp_code"
      ? "请先在 Cloudflare D1 控制台执行 scripts/sql/add-otp-code.sql，再重新部署。"
      : "请先手动补齐旧版数据库结构；0001 仅供新库初始化，不能重放到已有数据库。";
    throw new Error(`数据库缺少 ${missing.join("、")}。${hint} Worker 未发布`);
  }
  const emailColumns = schema[tables.indexOf("emails") + 1]!;
  for (const name of ["push_status", "otp_code"]) {
    const column = emailColumns.find(column => column["name"] === name)!;
    if (String(column["type"]).toUpperCase() !== "TEXT" || column["notnull"] !== 0 || column["dflt_value"] !== null || column["pk"] !== 0) {
      throw new Error(`emails.${name} 必须为可空 TEXT 且无默认值，Worker 未发布`);
    }
  }
  const indexes = schema[tables.length + 1]!;
  const dedup = indexes.find(index => index["name"] === "idx_emails_message_recipient");
  const dedupColumns = schema[tables.length + 2]!.map(column => column["name"]);
  if (dedup?.["unique"] !== 1 || dedup["partial"] !== 1 || dedupColumns.join(",") !== "message_id,recipient"
    || indexes.some(index => index["name"] === "idx_emails_message_id")) {
    throw new Error("邮件去重索引与按收件地址去重的结构不一致，请先手动完成旧版数据库升级，Worker 未发布");
  }
  console.log("    数据库结构检查通过；已有数据和迁移记录保持不变");
}
