/** 用真实 Wrangler + 本地 D1 验证部署迁移，不连接线上数据库。 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { applyMigrations } from "./migrations.ts";

const root = await mkdtemp(join(tmpdir(), "mailflare-migration-test-"));
const original = await readFile("migrations/0002_reliability.sql", "utf8");
try {
  let expectedSchema: unknown;
  for (const scenario of ["fresh", "old-0001", "historical-push", "manual-push"] as const) {
    const configPath = join(root, `${scenario}.toml`);
    const persistTo = join(root, scenario);
    await writeFile(configPath, `name = "migration-test"\n[[d1_databases]]\nbinding = "DB"\ndatabase_name = "mailflare"\ndatabase_id = "00000000-0000-0000-0000-000000000001"\nmigrations_dir = ${JSON.stringify(resolve("migrations"))}\n`);
    const query = (sql: string): Array<Record<string, unknown>> => {
      const sqlPath = join(root, "query.sql");
      writeFileSync(sqlPath, sql);
      const response = spawnSync("bun", ["x", "wrangler", "d1", "execute", "DB", "--config", configPath, "--local", "--persist-to", persistTo, "--file", sqlPath, "--json"], { encoding: "utf8" });
      assert.equal(response.status, 0, response.stderr || response.stdout);
      const results = JSON.parse(response.stdout) as Array<{ success: boolean; results: Array<Record<string, unknown>> }>;
      assert(results.every(result => result.success));
      return results.at(-1)!.results;
    };
    if (scenario !== "fresh") {
      query(await readFile("migrations/0001_init.sql", "utf8"));
      query("CREATE TABLE d1_migrations (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT UNIQUE, applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL); INSERT INTO d1_migrations (name) VALUES ('0001_init.sql'); INSERT INTO emails (id, subject, text_body, attachments) VALUES ('kept', 'existing mail', 'keep body', '[]');");
      if (scenario !== "old-0001") {
        query("ALTER TABLE emails ADD COLUMN push_status TEXT; UPDATE emails SET push_status='ok' WHERE id='kept';");
        if (scenario === "historical-push") query("INSERT INTO d1_migrations (name) VALUES ('0002_push_status.sql');");
      }
    }
    const options = { configPath, persistTo, local: true };
    await applyMigrations(options);
    const schema = query("SELECT type,name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' AND name NOT LIKE '_cf_%' AND name <> 'd1_migrations' ORDER BY type,name");
    if (scenario === "fresh") expectedSchema = schema;
    else {
      assert.deepEqual(schema, expectedSchema, `${scenario}: 结构应与全新建库一致`);
      assert.deepEqual(query("SELECT subject,text_body,attachments,push_status,storage_status,raw_key,deleted_at FROM emails WHERE id='kept'"), [{
        subject: "existing mail", text_body: "keep body", attachments: "[]",
        push_status: scenario === "old-0001" ? null : "ok", storage_status: "complete", raw_key: null, deleted_at: null,
      }]);
    }
    const ledger = query("SELECT * FROM d1_migrations ORDER BY id");
    assert.equal(ledger.filter(row => row["name"] === "0002_reliability.sql").length, 1);
    await applyMigrations(options);
    assert.deepEqual(query("SELECT * FROM d1_migrations ORDER BY id"), ledger, "重跑不重复记账");
    assert.equal(await readFile("migrations/0002_reliability.sql", "utf8"), original, "不能修改源码中的 SQL");
    console.log(`通过：${scenario} 升级、原数据保留、重复执行`);
  }
} finally {
  await rm(root, { recursive: true, force: true });
}
