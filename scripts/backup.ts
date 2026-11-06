#!/usr/bin/env bun
/** 流式备份正文、元数据、原始 EML 与已保存附件；不读取设置或密钥。 */
import { mkdir, chmod } from "node:fs/promises";
import { resolve, join } from "node:path";
import type { EmailDetail, EmailList, OkEnvelope } from "../web/src/api-types.ts";

const base = new URL(process.env["MAILFLARE_URL"] ?? "http://localhost:8787");
if (base.protocol !== "https:" && !["localhost", "127.0.0.1", "[::1]"].includes(base.hostname)) {
  throw new Error("远程备份必须使用 HTTPS");
}
const token = process.env["MAILFLARE_TOKEN"];
if (!token) throw new Error("请通过 MAILFLARE_TOKEN 环境变量提供登录 token");
const directory = resolve(process.argv[2] ?? `backups/${new Date().toISOString().replace(/[:.]/g, "-")}`);
await mkdir(resolve(directory, ".."), { recursive: true, mode: 0o700 });
await mkdir(directory, { mode: 0o700 }); // 已有目录直接失败，避免覆盖备份。
const seen = new Set<string>();
const missing: string[] = [];
async function request(path: string): Promise<Response> {
  const response = await fetch(new URL(path, base), { headers: { Authorization: `Bearer ${token}` }, redirect: "error", signal: AbortSignal.timeout(60_000) });
  if (!response.ok) throw new Error(`${path}: HTTP ${response.status}`);
  return response;
}
async function json<T>(path: string): Promise<T> {
  const envelope = await (await request(path)).json() as OkEnvelope<T>;
  if (!envelope.ok) throw new Error(`备份 API 返回失败: ${path}`);
  return envelope.data;
}
async function save(path: string, value: string | Response): Promise<void> {
  if (typeof value === "string") await Bun.write(path, value);
  else await Bun.write(path, value);
  await chmod(path, 0o600);
}
for (const filter of ["all", "trash"]) {
  let cursor: string | null = null;
  do {
    const params = new URLSearchParams({ filter, limit: "100" });
    if (cursor) params.set("cursor", cursor);
    const page = await json<EmailList>(`/api/emails?${params}`);
    for (const item of page.items) {
      if (!/^[a-zA-Z0-9-]{1,64}$/.test(item.id)) throw new Error("邮件 ID 格式无效");
      if (seen.has(item.id)) continue;
      seen.add(item.id);
      const email = await json<EmailDetail>(`/api/emails/${item.id}/export`);
      const folder = join(directory, item.id);
      await mkdir(folder, { mode: 0o700 });
      await save(join(folder, "email.json"), JSON.stringify(email, null, 2));
      if (email.raw_key) await save(join(folder, "original.eml"), await request(`/api/originals/${item.id}`));
      else missing.push(`${item.id}: 无历史原始邮件`);
      for (const attachment of email.attachments) {
        if (!Number.isSafeInteger(attachment.index) || attachment.index < 0) throw new Error("附件序号无效");
        if (attachment.stored) await save(join(folder, `attachment-${attachment.index}`), await request(`/api/attachments/${item.id}_${attachment.index}`));
        else missing.push(`${item.id}: 附件 ${attachment.index} 未单独保存${email.raw_key ? "（请从 original.eml 提取）" : ""}`);
      }
    }
    cursor = page.next_cursor;
  } while (cursor);
}
await save(join(directory, "manifest.json"), JSON.stringify({ format: "mailflare-backup-v1", at: new Date().toISOString(), count: seen.size, emails: [...seen], missing }, null, 2));
console.log(`备份完成：${seen.size} 封邮件 → ${directory}`);
if (missing.length) console.log(`${missing.length} 项历史原文或独立附件不可用，详情见 manifest.json`);
