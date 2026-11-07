/** 新交互的浏览器场景；所有 API 都由本地 fixture 响应。 */
import assert from "node:assert/strict";
import type { Browser } from "playwright-core";
import type { EmailDetail, SettingsView } from "../web/src/api-types.ts";

type Mail = { -readonly [K in keyof EmailDetail]: EmailDetail[K] };
function makeMail(id: string, patch: Partial<Mail> = {}): Mail {
  return { id, message_id: id, sender: "Sender <sender@example.test>", recipient: "inbox@example.test", subject: `邮件 ${id}`, text_body: "正文", html_body: "", attachments: [], is_read: false, created_at: Date.now(), push_status: null, original_url: null, raw_key: null, storage_status: "complete", deleted_at: null, attachment_urls: [], otp_code: null, ...patch };
}

async function harness(browser: Browser, html: string, hash = "") {
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 }, reducedMotion: "reduce" });
  const emails = [makeMail("first"), makeMail("second"), makeMail("pending", { storage_status: "pending", original_url: "/api/originals/pending", attachments: [{ index: 0, filename: "资料.pdf", content_type: "application/pdf", size: 1234, key: "", inline: false, stored: false }] }), makeMail("purging", { storage_status: "purging", deleted_at: Date.now() })];
  const settings: { -readonly [K in keyof SettingsView]: SettingsView[K] } = { site_name: "MailFlare", site_logo: "", site_background: "", turnstile_site_key: "", turnstile_secret_set: false, telegram_enabled: false, telegram_bot_token_set: false, telegram_chat_id: "", bark_enabled: false, bark_server: "https://api.day.app", bark_device_key_set: false, r2_account_id: "", r2_access_key_id_set: false, r2_secret_access_key_set: false, r2_bucket: "", blocked_senders: [], allowed_recipients: [], max_body_bytes: 262144, max_message_bytes: 20971520, max_attachment_bytes: 10485760, r2_concurrency: 4, page_size_default: 20, page_size_max: 100 };
  const errors: string[] = [];
  const mutations: string[] = [];
  const failures = new Set<string>();
  page.on("pageerror", error => errors.push(error.message));
  await page.addInitScript(() => localStorage.setItem("mf_token", "local-test-token"));
  await page.route("**/*", async route => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    const ok = (data: unknown) => route.fulfill({ json: { ok: true, data } });
    const fail = (error: string, status = 500) => route.fulfill({ status, json: { ok: false, error } });
    if (path === "/") return route.fulfill({ contentType: "text/html", body: html });
    if (request.method() !== "GET") mutations.push(`${request.method()} ${path}`);
    if (failures.has(path)) return fail("模拟请求失败");
    if (path === "/api/auth/session") return ok({ authenticated: true });
    if (path === "/api/auth/config") return ok({ ...settings, turnstile_required: false });
    if (path === "/api/settings/last-push") return ok({ last_push: null });
    if (path === "/api/settings") {
      if (request.method() === "PUT") Object.assign(settings, request.postDataJSON());
      return ok(settings);
    }
    if (path === "/api/emails") {
      const filter = url.searchParams.get("filter");
      const q = url.searchParams.get("q") ?? "";
      return ok({ items: emails.filter(mail => (filter === "trash") === (mail.deleted_at !== null) && (filter !== "unread" || !mail.is_read) && `${mail.subject} ${mail.sender} ${mail.recipient}`.includes(q)).map(mail => ({ ...mail, preview: "正文摘要", attachment_count: mail.attachments.length })), unread: emails.filter(mail => !mail.is_read && mail.deleted_at === null).length, next_cursor: null });
    }
    if (path === "/api/emails/read-all") { emails.forEach(mail => { if (mail.deleted_at === null) mail.is_read = true; }); return ok({}); }
    const [, , , id, action] = path.split("/");
    const mail = emails.find(mail => mail.id === id);
    if (path.startsWith("/api/emails/")) {
      if (!mail) return fail("邮件不存在", 404);
      if (action === "restore") { mail.deleted_at = null; return ok({}); }
      if (action === "read") { mail.is_read = Boolean(request.postDataJSON().is_read); return ok({}); }
      if (action === "purge") { emails.splice(emails.indexOf(mail), 1); return ok({}); }
      if (request.method() === "DELETE") { mail.deleted_at = Date.now(); return ok({}); }
      if (mail.deleted_at === null) mail.is_read = true;
      return ok(mail);
    }
    throw new Error(`未模拟的请求：${request.method()} ${path}`);
  });
  await page.goto(`https://mailflare.test/${hash}`);
  await page.locator("button.mail-item").first().waitFor();
  return { page, emails, settings, errors, mutations, failures };
}

export async function checkInteractions(browser: Browser, html: string): Promise<void> {
  const { page, emails, errors, mutations, failures } = await harness(browser, html);
  try {
    await page.getByRole("button", { name: /邮件 pending/ }).click();
    await page.locator(".notice").waitFor();
    assert((await page.locator(".notice").textContent())?.includes("导出原文"));
    assert.equal(await page.locator(".attachment-status").textContent(), "未保存");
    emails.find(mail => mail.id === "pending")!.original_url = null;
    await page.locator("#refresh").click();
    await page.waitForFunction(() => !document.querySelector(".notice")?.textContent?.includes("导出原文"));
    await page.getByRole("button", { name: "删除", exact: true }).click();
    await page.getByText("已移入回收站", { exact: true }).waitFor();
    await page.getByRole("button", { name: "回收站", exact: true }).click();
    await page.getByRole("button", { name: /邮件 pending/ }).click();
    assert(await page.getByRole("button", { name: "彻底删除", exact: true }).isDisabled());
    await page.getByRole("button", { name: /邮件 purging/ }).click();
    await page.getByRole("button", { name: "恢复", exact: true }).waitFor();
    assert(await page.getByRole("button", { name: "恢复", exact: true }).isDisabled());
    assert((await page.locator(".notice").textContent())?.includes("期间无法恢复"));
    await page.getByRole("button", { name: "全部邮件", exact: true }).click();
    await page.getByRole("button", { name: /邮件 first/ }).click();
    await page.getByRole("button", { name: "删除", exact: true }).click();
    await page.getByRole("button", { name: "撤销", exact: true }).click();
    await page.getByText("已恢复到收件箱", { exact: true }).waitFor();
    assert.equal(emails.find(mail => mail.id === "first")!.deleted_at, null);
    assert.equal(mutations.filter(path => path === "POST /api/emails/first/restore").length, 1);
    assert.equal(await page.locator(".detail-subject").textContent(), "邮件 first");
    await page.getByRole("button", { name: "删除", exact: true }).click();
    failures.add("/api/emails/first/restore");
    await page.getByRole("button", { name: "撤销", exact: true }).click();
    await page.getByText("模拟请求失败", { exact: true }).waitFor();
    assert.notEqual(emails.find(mail => mail.id === "first")!.deleted_at, null, "恢复失败不能假装成功");
    failures.clear();
    await page.locator("#open-settings").click();
    await page.locator("#settings-title").waitFor();
    assert(await page.locator("#settings-save").isDisabled());
    await page.locator("#site-name").fill("新站点");
    assert(await page.locator("#settings-dirty").isVisible());
    assert(await page.locator("#settings-save").isEnabled());
    await page.locator("#site-name").fill("MailFlare");
    assert(await page.locator("#settings-save").isDisabled(), "改回原值时无需保存");
    assert(await page.locator("#settings-dirty").isHidden());
    await page.locator("#bark-enabled").check();
    assert(await page.locator("#settings-save").isEnabled());
    await page.locator("#bark-enabled").uncheck();
    assert(await page.locator("#settings-save").isDisabled());
    await page.locator("#site-name").fill("我的邮箱");
    await page.locator("#open-settings").click();
    await page.getByRole("button", { name: "取消", exact: true }).click();
    assert.equal(await page.locator("#site-name").inputValue(), "我的邮箱");
    failures.add("/api/settings");
    await page.locator("#settings-save").click();
    await page.locator("#settings-err").getByText("模拟请求失败", { exact: true }).waitFor();
    assert(await page.locator("#settings-save").isEnabled());
    assert(await page.locator("#settings-dirty").isVisible());
    failures.clear();
    await page.locator("#settings-save").click();
    await page.getByText("设置已保存", { exact: true }).waitFor();
    assert(await page.locator("#settings-save").isDisabled());
    assert(await page.locator("#settings-dirty").isHidden());
    await page.locator("#site-name").fill("放弃这个名字");
    await page.locator("#open-settings").click();
    await page.getByRole("button", { name: "放弃更改", exact: true }).click();
    await page.locator("#settings-view").waitFor({ state: "hidden" });
    await page.locator("#open-settings").click();
    await page.locator("#settings-title").waitFor();
    assert.equal(await page.locator("#site-name").inputValue(), "我的邮箱");
    assert(await page.locator("#settings-save").isDisabled());
    assert.deepEqual(errors, []);
    console.log("通过：存储状态、撤销、设置修改还原/失败重试/保存/离开确认（Chromium）");
  } finally { await page.close(); }
  await checkKeyboard(browser, html);
  await checkNavigation(browser, html);
}

async function checkNavigation(browser: Browser, html: string): Promise<void> {
  const { page, errors } = await harness(browser, html);
  try {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.getByRole("button", { name: /邮件 first/ }).click();
    await page.locator(".detail-subject").getByText("邮件 first", { exact: true }).waitFor();
    assert.equal(new URL(page.url()).hash, "#all/first");
    await page.reload();
    await page.locator(".detail-subject").getByText("邮件 first", { exact: true }).waitFor();
    await page.goBack();
    await page.locator("#list-pane").waitFor({ state: "visible" });
    assert.equal(new URL(page.url()).hash, "#all");
    await page.goForward();
    await page.locator(".detail-subject").getByText("邮件 first", { exact: true }).waitFor();
    await page.getByRole("button", { name: "返回", exact: true }).click();
    await page.locator("#list-pane").waitFor({ state: "visible" });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.getByRole("button", { name: /邮件 first/ }).click();
    await page.locator(".detail-subject").getByText("邮件 first", { exact: true }).waitFor();
    const length = await page.evaluate(() => history.length);
    await page.keyboard.press("j");
    await page.locator(".detail-subject").getByText("邮件 second", { exact: true }).waitFor();
    assert.equal(new URL(page.url()).hash, "#all/second");
    assert.equal(await page.evaluate(() => history.length), length, "切换邮件不能堆积历史");
    await page.locator("#open-settings").click();
    await page.locator("#site-name").fill("尚未保存");
    await page.goBack();
    await page.getByRole("button", { name: "取消", exact: true }).click();
    await page.waitForURL("**/#all/second");
    assert.equal(await page.locator("#site-name").inputValue(), "尚未保存");
    assert.equal(await page.evaluate(() => history.length), length, "取消返回应回到原条目");
    await page.goBack();
    await page.getByRole("dialog").waitFor();
    await page.goForward();
    await page.getByRole("button", { name: "取消", exact: true }).click();
    await page.waitForURL("**/#all/second");
    assert.equal(await page.locator("#site-name").inputValue(), "尚未保存", "确认期间返回原历史条目，取消后不得刷新丢失草稿");
    await page.goBack();
    await page.getByRole("button", { name: "放弃更改", exact: true }).click();
    await page.locator("#settings-view").waitFor({ state: "hidden" });
    assert.equal(new URL(page.url()).hash, "#all");
    await page.goForward();
    await page.locator(".detail-subject").getByText("邮件 second", { exact: true }).waitFor();
    await page.evaluate(() => { location.hash = "#all/missing"; });
    await page.getByText("邮件不存在", { exact: true }).waitFor();
    await page.waitForURL("**/#all");
    assert.equal(await page.locator(".detail-subject").count(), 0);
    await page.evaluate(() => { location.hash = "#trash/purging"; });
    await page.locator(".detail-subject").getByText("邮件 purging", { exact: true }).waitFor();
    await page.reload();
    await page.locator(".detail-subject").getByText("邮件 purging", { exact: true }).waitFor();
    assert.equal(await page.locator("#list-title").textContent(), "回收站");
    assert.deepEqual(errors, []);
  } finally { await page.close(); }
  const direct = await harness(browser, html, "#all/first");
  try {
    await direct.page.setViewportSize({ width: 390, height: 844 });
    await direct.page.locator(".detail-subject").getByText("邮件 first", { exact: true }).waitFor();
    await direct.page.goBack();
    await direct.page.locator("#list-pane").waitFor({ state: "visible" });
    assert.equal(new URL(direct.page.url()).hash, "#all", "直接打开详情也能返回列表");
    assert.deepEqual(direct.errors, []);
  } finally { await direct.page.close(); }
  console.log("通过：手机返回/前进/刷新、直接链接、取消离开设置、失效链接与回收站路由（Chromium）");
}

async function checkKeyboard(browser: Browser, html: string): Promise<void> {
  const { page, emails, settings, errors, mutations } = await harness(browser, html);
  try {
    assert.equal(await page.title(), "(3) MailFlare");
    emails[0]!.text_body = "长正文\n".repeat(200);
    await page.getByRole("button", { name: /邮件 first/ }).click();
    await page.waitForFunction(() => document.title === "(2) MailFlare");
    await page.locator("#detail-pane").evaluate(node => { node.scrollTop = 500; });
    await page.keyboard.press("j");
    await page.locator(".detail-subject").getByText("邮件 second", { exact: true }).waitFor();
    assert.equal(await page.title(), "(1) MailFlare");
    assert.equal(await page.locator("#detail-pane").evaluate(node => node.scrollTop), 0);
    assert.equal(await page.evaluate(() => (document.activeElement as HTMLElement).dataset["mailId"]), "second");
    await page.keyboard.press("ArrowUp");
    await page.locator(".detail-subject").getByText("邮件 first", { exact: true }).waitFor();
    await page.locator("#search").focus();
    await page.keyboard.press("ArrowDown");
    assert.equal(await page.locator(".detail-subject").textContent(), "邮件 first");
    await page.locator(".mail-item.selected").focus();
    await page.keyboard.press("Control+j");
    assert.equal(await page.locator(".detail-subject").textContent(), "邮件 first");
    await page.keyboard.press("Delete");
    await page.getByRole("button", { name: "撤销", exact: true }).waitFor();
    assert.notEqual(emails[0]!.deleted_at, null);
    await page.getByRole("button", { name: "撤销", exact: true }).click();
    await page.getByText("已恢复到收件箱", { exact: true }).waitFor();
    settings.site_name = "我的站点";
    await page.locator("#open-settings").click();
    await page.locator("#settings-title").waitFor();
    assert.equal(await page.title(), "(1) 我的站点", "修改站点名称不能抹掉未读数");
    const count = mutations.length;
    await page.keyboard.press("Delete");
    assert.equal(mutations.length, count, "设置页不触发删除快捷键");
    await page.locator("#open-settings").click();
    await page.getByRole("button", { name: "回收站", exact: true }).click();
    await page.getByRole("button", { name: /邮件 purging/ }).click();
    assert.equal(await page.title(), "(1) 我的站点", "回收站仍使用收件箱未读总数");
    await page.keyboard.press("Delete");
    await page.getByRole("dialog").waitFor();
    await page.keyboard.press("j");
    await page.keyboard.press("Escape");
    await page.getByRole("dialog").waitFor({ state: "hidden" });
    assert.equal(await page.locator(".detail-subject").textContent(), "邮件 purging");
    assert(!mutations.some(path => path.endsWith("/purge")), "彻底删除必须经过确认");
    assert.deepEqual(errors, []);
    console.log("通过：未读标题、站点名更新、键盘导航/删除/焦点/滚动与弹窗保护（Chromium）");
  } finally { await page.close(); }
}
