/** 测试实际构建出的界面；API 和下载都在浏览器路由中模拟。 */
import assert from "node:assert/strict";
import { chromium } from "playwright-core";
import type { Route } from "playwright-core";
const browser = await chromium.launch({ executablePath: process.env["CHROMIUM_PATH"] ?? "/usr/bin/chromium", args: ["--no-sandbox"] });
const html = await Bun.file("public/index.html").text();
const email = { id: "demo", sender: "Sender <sender@example.test>", recipient: "account-notifications+november@long-subdomain.example.test", subject: "界面测试邮件：这是一封包含较长标题和完整收件地址的通知", preview: "这是一段很长的广告正文预览，不应挤占收件地址的空间。".repeat(8), is_read: false, created_at: Date.now(), attachment_count: 2, message_id: "demo@example.test", text_body: "hello", html_body: "", attachments: [], attachment_urls: [], otp_code: null, push_status: null, raw_key: "mail/demo/original.eml", original_url: "/api/originals/demo?token=local", storage_status: "complete", deleted_at: null as number | null };
let oldSearch: Route | undefined;
email.html_body = '<p>hello</p><img src="https://tracker.test/ui-pixel">';
try {
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 }, reducedMotion: "reduce" });
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
  await page.locator(".mail-item").waitFor();
  // 验证实际排版：长摘要、长地址和附件同时出现时，地址仍有可读宽度。
  for (const width of [1440, 1280, 1100, 1024, 861, 860, 768, 390, 320]) {
    await page.setViewportSize({ width, height: 900 });
    const layout = await page.locator(".mail-item").evaluate(row => {
      const address = row.querySelector<HTMLElement>(".recipient-address")!;
      const recipient = row.querySelector<HTMLElement>(".mail-recipient")!;
      const snippet = row.querySelector<HTMLElement>(".snippet")!;
      const attachment = row.querySelector<HTMLElement>(".attachment-tag")!;
      const rect = address.getBoundingClientRect();
      return {
        width: rect.width,
        separateLine: rect.top >= snippet.getBoundingClientRect().bottom,
        contained: rect.right <= row.getBoundingClientRect().right && rect.right <= attachment.getBoundingClientRect().left,
        ellipsis: getComputedStyle(address).textOverflow,
        title: recipient.title,
        pageFits: document.documentElement.scrollWidth <= innerWidth,
        sidebarWidth: document.querySelector("#sidebar")!.getBoundingClientRect().width,
      };
    });
    assert(layout.width >= 140, `${width}px: 收件地址不能被压到只剩标签`);
    assert(layout.separateLine && layout.contained && layout.pageFits, `${width}px: 地址、摘要和附件不能重叠或溢出`);
    assert.equal(layout.ellipsis, "ellipsis", "过长地址应明确省略，不能直接裁切");
    assert.equal(layout.title, `收件地址：${email.recipient}`, "悬停能查看完整地址");
    if (width > 860) assert(layout.sidebarWidth <= 160, "桌面导航应收窄");
  }
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.getByRole("button", { name: /界面测试邮件/ }).click();
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"], { origin: "https://mailflare.test" });
  await page.getByRole("button", { name: "复制收件地址" }).click();
  await page.getByText("收件地址已复制", { exact: true }).waitFor();
  assert.equal(await page.evaluate(() => navigator.clipboard.readText()), email.recipient);
  for (const width of [1280, 1024, 861, 390, 320]) {
    await page.setViewportSize({ width, height: 900 });
    const layout = await page.locator("#detail-pane").evaluate(pane => {
      const rect = pane.getBoundingClientRect();
      const subject = pane.querySelector<HTMLElement>(".detail-subject")!;
      const toolbar = pane.querySelector<HTMLElement>(".detail-toolbar")!;
      const buttons = [...toolbar.querySelectorAll<HTMLElement>("button, a")].filter(node => node.getBoundingClientRect().width > 0);
      return {
        titleFits: subject.scrollWidth <= subject.clientWidth,
        actionsFit: buttons.every(node => { const r = node.getBoundingClientRect(); return r.left >= rect.left && r.right <= rect.right; }),
        separateTitle: subject.getBoundingClientRect().top >= toolbar.getBoundingClientRect().bottom,
        pageFits: document.documentElement.scrollWidth <= innerWidth,
      };
    });
    assert(layout.titleFits && layout.actionsFit && layout.separateTitle && layout.pageFits, `${width}px: 长标题和操作栏应完整显示`);
  }
  await page.setViewportSize({ width: 1280, height: 900 });
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
  console.log("通过：320–1440px 收件地址与附件布局、长标题操作栏、复制完整地址、禁图提示与主动加载、删除/恢复/原文导出、搜索乱序、手机导航、人机验证隐藏与失效恢复");
} finally { await browser.close(); }
