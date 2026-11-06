/** 测试实际构建出的界面；API 和下载都在浏览器路由中模拟。 */
import assert from "node:assert/strict";
import { chromium } from "playwright-core";
import type { Route } from "playwright-core";
const browser = await chromium.launch({ executablePath: process.env["CHROMIUM_PATH"] ?? "/usr/bin/chromium", args: ["--no-sandbox"] });
const html = await Bun.file("public/index.html").text();
const email = { id: "demo", sender: "Sender <sender@example.test>", recipient: "inbox@example.test", subject: "界面测试邮件", preview: "hello", is_read: false, created_at: Date.now(), attachment_count: 0, message_id: "demo@example.test", text_body: "hello", html_body: "", attachments: [], attachment_urls: [], otp_code: null, push_status: null, raw_key: "mail/demo/original.eml", original_url: "/api/originals/demo?token=local", storage_status: "complete", deleted_at: null as number | null };
let oldSearch: Route | undefined;
email.html_body = '<p>hello</p><img src="https://tracker.test/ui-pixel">';
try {
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  const errors: string[] = [];
  let imageRequests = 0;
  page.on("pageerror", error => errors.push(error.message));
  await page.addInitScript(() => localStorage.setItem("mf_token", "local-test-token"));
  await page.route("**/*", async route => {
    const request = route.request();
    const url = new URL(request.url());
    const ok = (data: unknown) => route.fulfill({ json: { ok: true, data } });
    if (url.href === "https://tracker.test/ui-pixel") { imageRequests += 1; return route.abort(); }
    if (url.pathname === "/") return route.fulfill({ contentType: "text/html", body: html });
    if (url.pathname === "/api/settings/last-push") return ok({ last_push: null });
    if (url.pathname === "/api/auth/session") return ok({ authenticated: true });
    if (url.pathname === "/api/auth/config") return ok({ site_name: "MailFlare", site_logo: "", site_background: "", turnstile_required: false });
    if (url.pathname === "/api/emails") {
      const q = url.searchParams.get("q");
      if (q === "old") { oldSearch = route; return; }
      if (q === "new") return ok({ items: [{ ...email, id: "new", subject: "新搜索结果" }], unread: 0, next_cursor: null });
      const trash = url.searchParams.get("filter") === "trash";
      return ok({ items: trash === (email.deleted_at !== null) ? [email] : [], unread: email.is_read || email.deleted_at ? 0 : 1, next_cursor: null });
    }
    if (url.pathname === "/api/emails/demo") {
      if (request.method() === "DELETE") { email.deleted_at = Date.now(); return ok({ deleted: true }); }
      if (email.deleted_at === null) email.is_read = true;
      return ok(email);
    }
    if (url.pathname === "/api/emails/demo/restore") { email.deleted_at = null; return ok({ restored: true }); }
    throw new Error(`未模拟的请求: ${request.method()} ${url.pathname}`);
  });
  await page.goto("https://mailflare.test/");
  await page.getByRole("button", { name: /界面测试邮件/ }).click();
  assert.equal(await page.getByRole("link", { name: "导出原文" }).getAttribute("href"), email.original_url);
  await page.frameLocator("iframe.mail-html").getByText("hello", { exact: true }).waitFor();
  assert.equal(imageRequests, 0, "打开邮件时远程图片不能加载");
  assert.equal(await page.getByRole("button", { name: "显示图片", exact: true }).count(), 1, "禁图提示只能出现一次");
  await Promise.all([
    page.waitForRequest("https://tracker.test/ui-pixel"),
    page.getByRole("button", { name: "显示图片", exact: true }).click(),
  ]);
  assert.equal(await page.locator(".imgbar").count(), 0);
  await page.getByRole("button", { name: "删除", exact: true }).click();
  await page.getByText("已移入回收站", { exact: true }).waitFor();
  await page.getByRole("button", { name: "回收站", exact: true }).click();
  await page.getByRole("button", { name: /界面测试邮件/ }).click();
  await page.getByRole("button", { name: "彻底删除", exact: true }).waitFor();
  assert.equal(await page.getByRole("button", { name: "显示图片", exact: true }).count(), 1, "重新打开邮件恢复禁图");
  assert(await page.locator("#mark-all-read").isDisabled());
  await page.getByRole("button", { name: "恢复", exact: true }).click();
  await page.getByText("回收站是空的", { exact: true }).waitFor();
  await page.getByRole("button", { name: "全部邮件", exact: true }).click();
  await page.getByRole("button", { name: /界面测试邮件/ }).waitFor();

  await page.locator("#search").fill("old");
  await page.waitForRequest(request => new URL(request.url()).searchParams.get("q") === "old");
  await page.locator("#search").fill("new");
  await page.getByRole("button", { name: /新搜索结果/ }).waitFor();
  assert(oldSearch);
  await oldSearch.fulfill({ json: { ok: true, data: { items: [{ ...email, subject: "过时搜索结果" }], next_cursor: null, unread: 0 } } });
  await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
  assert((await page.locator("#list-pane").textContent())?.includes("新搜索结果"));
  assert.equal(await page.getByRole("button", { name: /过时搜索结果/ }).count(), 0);
  await page.locator("#search").fill("");
  await page.getByRole("button", { name: /界面测试邮件/ }).waitFor();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator("#toggle-sidebar").click();
  await page.getByRole("button", { name: "回收站", exact: true }).click();
  await page.getByText("回收站是空的", { exact: true }).waitFor();
  assert.equal(await page.locator("main").evaluate(node => node instanceof HTMLElement && node.inert), false);
  assert.deepEqual(errors, []);
  await page.close();

  // 模拟 Turnstile 回调，验证实际登录页的隐藏、过期恢复与失败重置。
  type TurnstileHarness = Window & {
    turnstileCallbacks?: Parameters<NonNullable<Window["turnstile"]>["render"]>[1];
    visibleAtReset?: boolean;
  };
  const login = await browser.newPage();
  const loginErrors: string[] = [];
  login.on("pageerror", error => loginErrors.push(error.message));
  await login.addInitScript(() => {
    const harness = window as TurnstileHarness;
    window.turnstile = {
      render(_container, options) { harness.turnstileCallbacks = options; return "test-widget"; },
      reset() { harness.visibleAtReset = !document.querySelector<HTMLElement>("#turnstile-field")!.hidden; },
    };
  });
  const submittedTokens: string[] = [];
  await login.route("**/*", async route => {
    const url = new URL(route.request().url());
    if (url.pathname === "/") return route.fulfill({ contentType: "text/html", body: html });
    if (url.pathname === "/api/auth/config") return route.fulfill({ json: { ok: true, data: { site_name: "MailFlare", site_logo: "", site_background: "", turnstile_required: true, turnstile_site_key: "test-site-key" } } });
    if (url.pathname === "/api/auth/login") {
      submittedTokens.push((route.request().postDataJSON() as { turnstile_token: string }).turnstile_token);
      return route.fulfill({ status: 401, json: { ok: false, error: "测试登录失败" } });
    }
    throw new Error(`未模拟的登录请求: ${url.href}`);
  });
  await login.goto("https://mailflare.test/");
  await login.waitForFunction(() => Boolean((window as TurnstileHarness).turnstileCallbacks));
  const field = login.locator("#turnstile-field");
  assert(await field.isVisible());
  await login.locator("#username").fill("test-user");
  await login.locator("#password").fill("test-password");
  for (const event of ["expired-callback", "error-callback", "timeout-callback"] as const) {
    await login.evaluate(() => (window as TurnstileHarness).turnstileCallbacks!.callback!("valid-token"));
    assert(await field.isHidden(), "验证通过应收起整个验证区域");
    assert.equal(await field.evaluate(node => node.getBoundingClientRect().height), 0, "隐藏后不能保留空白占位");
    await login.evaluate(event => (window as TurnstileHarness).turnstileCallbacks![event]!(), event);
    assert(await field.isVisible(), `${event} 应重新显示验证区域`);
    await login.locator("#login-btn").click();
    await login.waitForFunction(() => !document.querySelector<HTMLButtonElement>("#login-btn")!.disabled);
    assert.equal(submittedTokens.at(-1), "", "失效的 token 不能继续提交");
  }
  await login.evaluate(() => (window as TurnstileHarness).turnstileCallbacks!.callback!("fresh-token"));
  assert(await field.isHidden());
  await login.locator("#login-btn").click();
  await field.waitFor({ state: "visible" });
  assert.equal(submittedTokens.at(-1), "fresh-token");
  assert(await login.evaluate(() => (window as TurnstileHarness).visibleAtReset), "登录失败必须先显示挂件再重置");
  assert.deepEqual(loginErrors, []);
  console.log("通过：禁图提示与主动加载、删除/恢复/原文导出、搜索乱序、手机导航、人机验证隐藏与失效恢复");
} finally { await browser.close(); }
