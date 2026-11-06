/** 在真实浏览器中验证 CSP 的解析结果及请求行为，全部外网请求被拦截。 */
import assert from "node:assert/strict";
import { chromium } from "playwright-core";

const result = await Bun.build({ entrypoints: ["web/src/mail-render.ts"], target: "browser", format: "esm" });
if (!result.success) throw new Error("渲染模块构建失败");
const moduleCode = await result.outputs[0]!.text();
const browser = await chromium.launch({ executablePath: process.env["CHROMIUM_PATH"] ?? "/usr/bin/chromium", args: ["--no-sandbox"] });
try {
  const page = await browser.newPage();
  const remoteRequests: string[] = [];
  await page.route("**/*", async (route) => {
    const url = route.request().url();
    if (url === "https://mailflare.test/") await route.fulfill({ contentType: "text/html", body: "<!doctype html><html><body></body></html>" });
    else if (url === "https://mailflare.test/render.js") await route.fulfill({ contentType: "application/javascript", body: moduleCode });
    else { remoteRequests.push(url); await route.abort(); }
  });
  await page.goto("https://mailflare.test/");
  const result = await page.evaluate(async () => {
    const render: typeof import("../web/src/mail-render.ts") = await import(String("/render.js"));
    const cases = [
      '<!-- <head> --><div><img src="https://tracker.test/pixel"></div>',
      '<html><head></head><body><img src="//tracker.test/pixel"></body></html>',
      '<div title="<head>"><img srcset="https://tracker.test/pixel 2x"></div>',
      '<meta http-equiv="refresh" content="0;url=https://tracker.test/redirect"><base href="https://tracker.test/"><iframe src="https://tracker.test/frame"></iframe>',
      '<style>body{background:url(https://tracker.test/css)}</style><script>parent.pwned=true</script><img src=x onerror="parent.pwned=true">',
      '<textarea><head></textarea><a href=https://example.test rel=noopenerfake target=_top ping=https://tracker.test/ping>link</a>',
      '<math><mtext><table><mglyph><style><!--</style><img title="--><img src=https://tracker.test/mutation onerror=parent.pwned=true>">',
    ];
    const failures: string[] = [];
    for (const [i, html] of cases.entries()) {
      const frame = document.createElement("iframe");
      frame.setAttribute("sandbox", "allow-same-origin allow-popups allow-popups-to-escape-sandbox");
      const loaded = new Promise<void>((resolve) => frame.addEventListener("load", () => resolve(), { once: true }));
      frame.srcdoc = render.buildSrcdoc(html, false);
      document.body.append(frame);
      await loaded;
      const doc = frame.contentDocument!;
      if (!doc.head.querySelector('meta[http-equiv="Content-Security-Policy"]')) failures.push(`${i}: CSP 不在 head`);
      if (doc.querySelector("iframe, script, base, meta[http-equiv=refresh]")) failures.push(`${i}: 活跃内容未移除`);
      const link = doc.querySelector("a[href]");
      if (link && (!link.getAttribute("rel")?.split(/\s+/).includes("noopener") || link.getAttribute("target") !== "_blank")) failures.push(`${i}: 链接保护失败`);
      frame.remove();
    }
    const links = document.createElement("template");
    links.innerHTML = render.buildSrcdoc('<a href="jav&#x61;script:alert(1)" rel=opener>x</a><a href="#x">anchor</a>', false);
    const [unsafe, anchor] = links.content.querySelectorAll("a");
    if (!unsafe || unsafe.hasAttribute("href") || unsafe.relList.contains("opener") || !unsafe.relList.contains("noopener")) failures.push("危险链接");
    if (anchor?.getAttribute("href") !== "#x") failures.push("页内锚点被移除");
    if (!render.hasRemoteImages('<img src="//tracker.test/x">')) failures.push("协议相对图片检测");
    if (!render.hasRemoteImages('<img srcset="https://tracker.test/x 2x">')) failures.push("srcset 图片检测");
    if (render.hasRemoteImages('<img src="data:image/png;base64,AAAA">')) failures.push("内嵌图片误判");
    render.buildSrcdoc('<img src="https://tracker.test/parse"><script>parent.pwned=true</script>hello', true);
    return { failures, pwned: "pwned" in window };
  });
  assert.deepEqual(result.failures, []);
  assert.equal(result.pwned, false);
  assert.deepEqual(remoteRequests, [], "默认渲染和解析不能发出外部请求");
  const allowedRequest = page.waitForRequest("https://tracker.test/allowed");
  await page.evaluate(async () => {
    const render: typeof import("../web/src/mail-render.ts") = await import(String("/render.js"));
    const frame = document.createElement("iframe");
    frame.setAttribute("sandbox", "allow-same-origin");
    frame.srcdoc = render.buildSrcdoc('<img src="https://tracker.test/allowed">', true);
    document.body.append(frame);
  });
  await allowedRequest;
  console.log("通过：畸形 HTML、CSP、禁图、链接属性和用户主动显示图片（真实 Chromium）");
} finally { await browser.close(); }
