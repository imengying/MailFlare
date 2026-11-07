/** 用真实 Wrangler + 本地 D1 验证初始化、手动加列与只读检查，不连接线上数据库。 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { prepareDatabase } from "./migrations.ts";

const root = await mkdtemp(join(tmpdir(), "mailflare-schema-test-"));
const initial = await readFile("migrations/0001_init.sql", "utf8");
const manual = await readFile("scripts/sql/add-otp-code.sql", "utf8");
const previous = initial.replace(/,\n  otp_code\s+TEXT\n/, "\n");
assert.notEqual(previous, initial, "旧库 fixture 不应包含 otp_code");
try {
  for (const scenario of ["fresh", "manual-upgrade", "incomplete", "wrong-type"] as const) {
    const configPath = join(root, `${scenario}.toml`);
    const persistTo = join(root, scenario);
    await writeFile(configPath, `name = "schema-test"\n[[d1_databases]]\nbinding = "DB"\ndatabase_name = "mailflare"\ndatabase_id = "00000000-0000-0000-0000-000000000001"\nmigrations_dir = ${JSON.stringify(resolve("migrations"))}\n`);
    const query = (sql: string): Array<Record<string, unknown>> => {
      const sqlPath = join(root, "query.sql");
      writeFileSync(sqlPath, sql);
      const response = spawnSync("bun", ["x", "wrangler", "d1", "execute", "DB", "--config", configPath, "--local", "--persist-to", persistTo, "--file", sqlPath, "--json"], { encoding: "utf8" });
      assert.equal(response.status, 0, response.stderr || response.stdout);
      const results = JSON.parse(response.stdout) as Array<{ success: boolean; results: Array<Record<string, unknown>> }>;
      assert(results.every(result => result.success));
      return results.at(-1)!.results;
    };
    const options = { configPath, persistTo, local: true };
    if (scenario === "fresh") {
      await prepareDatabase(options);
      assert(query("PRAGMA table_info(emails)").some(column => column["name"] === "otp_code"));
      query("INSERT INTO emails (id, subject, otp_code) VALUES ('kept', 'Verification code: 0042', '0042');");
    } else {
      query(previous);
      query("CREATE TABLE d1_migrations (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT UNIQUE, applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL); INSERT INTO d1_migrations (name) VALUES ('0001_init.sql'), ('0002_push_status.sql'), ('0002_reliability.sql'); INSERT INTO emails (id, subject, text_body, push_status) VALUES ('kept', 'Verification code: 0042', 'keep body', 'ok');");
      if (scenario === "manual-upgrade") {
        const before = query("SELECT * FROM emails");
        const ledger = query("SELECT * FROM d1_migrations ORDER BY id");
        await assert.rejects(prepareDatabase(options), /scripts\/sql\/add-otp-code.sql/);
        assert.deepEqual(query("SELECT * FROM emails"), before, "缺列时不能修改历史邮件");
        assert.deepEqual(query("SELECT * FROM d1_migrations ORDER BY id"), ledger, "检查不能伪造迁移记录");
        query(manual);
        assert.deepEqual(query("SELECT * FROM emails"), before.map(row => ({ ...row, otp_code: null })), "手动加列仅增加可空字段");
      } else if (scenario === "incomplete") {
        query("DROP TABLE inbound_jobs;");
        await assert.rejects(prepareDatabase(options), /inbound_jobs/);
      } else {
        query("ALTER TABLE emails ADD COLUMN otp_code INTEGER;");
        await assert.rejects(prepareDatabase(options), /otp_code 必须为可空 TEXT/);
      }
    }
    if (scenario === "fresh" || scenario === "manual-upgrade") {
      const ledger = query("SELECT * FROM d1_migrations ORDER BY id");
      const rows = query("SELECT * FROM emails");
      await prepareDatabase(options);
      assert.deepEqual(query("SELECT * FROM d1_migrations ORDER BY id"), ledger, "重跑不能更改历史记账");
      assert.deepEqual(query("SELECT * FROM emails"), rows, "重跑不能更改邮件数据");
      if (scenario === "fresh") assert.deepEqual(ledger.map(row => row["name"]), ["0001_init.sql"]);
      if (scenario === "manual-upgrade") {
        query("CREATE UNIQUE INDEX idx_emails_message_id ON emails (message_id) WHERE message_id IS NOT NULL AND message_id <> ''; ");
        await assert.rejects(prepareDatabase(options), /去重索引/);
      }
    }
    console.log(`通过：${scenario}`);
  }
  assert.equal(await readFile("migrations/0001_init.sql", "utf8"), initial, "部署不能改写初始化 SQL");
} finally {
  await rm(root, { recursive: true, force: true });
}
