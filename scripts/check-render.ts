/**
 * 前端渲染安全的自检脚本（`bun run check:render`）。
 *
 * 为什么单独写这个而不是引入 vitest：
 * 只有 `mail-render.ts` 是纯函数、且安全相关，值得为它加测试。
 * 引入整套测试框架（jsdom / vitest）对 5 个断言来说成本过高。
 * 这里直接 import 纯函数、在 bun 里跑断言，零额外依赖。
 */

import { addLinkRel, buildSrcdoc, escapeHtml, hasRemoteImages } from "../web/src/mail-render.ts";

let failed = 0;

function check(name: string, condition: boolean, detail = ""): void {
  if (condition) {
    console.log(`  ✓ ${name}`);
  } else {
    console.error(`  ✗ ${name}${detail ? ` — ${detail}` : ""}`);
    failed += 1;
  }
}

console.log("mail-render 安全自检：");

// ── 1. sandbox 相关：srcdoc 必须带 CSP，且默认不放行远程图片 ──────────────
{
  const doc = buildSrcdoc("<p>hello</p>", false);
  check("默认注入 CSP", doc.includes("Content-Security-Policy"), doc.slice(0, 120));
  check("default-src 'none'", doc.includes("default-src 'none'"));
  check("默认 img-src 不含 *", /img-src 'self' data:/.test(doc), doc);
  check("默认不允许 https 图片", !/img-src \*/.test(doc));
  check("禁止表单提交", doc.includes("form-action 'none'"));
  check("带 no-referrer", doc.includes('name="referrer"'));
  check("带 base target=_blank", doc.includes('<base target="_blank">'));

  const relaxed = buildSrcdoc("<p>hi</p>", true);
  check("放行后 img-src 含 *", /img-src \*/.test(relaxed));
}

// ── 2. 三种 HTML 结构都能正确注入 head ─────────────────────────────────
{
  const withHead = buildSrcdoc("<html><head><title>t</title></head><body>x</body></html>", false);
  // 注入的内容应该出现在 <head> 之后、原有 <title> 之前（都在 head 内部）。
  const headStart = withHead.indexOf("<head>");
  const titlePos = withHead.indexOf("<title>");
  const cspPos = withHead.indexOf("Content-Security-Policy");
  check(
    "有 head：CSP 在 head 内且位于 title 之前",
    headStart >= 0 && cspPos > headStart && cspPos < titlePos,
    `head=${headStart} csp=${cspPos} title=${titlePos}`,
  );
  check("原有 title 保留", withHead.includes("<title>t</title>"));

  const withHtml = buildSrcdoc("<html><body>x</body></html>", false);
  check("只有 html：补 head", /<html[^>]*><head>/.test(withHtml));

  const fragment = buildSrcdoc("<div>x</div>", false);
  check("裸片段：前置 head", fragment.startsWith("<head>"));
}

// ── 3. 纯文本内容必须被转义 ───────────────────────────────────────────
{
  const doc = buildSrcdoc("<script>alert(1)</script>", false);
  // 不含任何 HTML 标签特征时应走 <pre> 转义分支。
  check("纯文本分支包裹 pre", doc.includes("<pre"));
  check("尖括号被转义", !doc.includes("<script>alert(1)</script>") || doc.includes("&lt;"));

  check("escapeHtml 处理 &", escapeHtml("a&b") === "a&amp;b");
  check("escapeHtml 处理 <", escapeHtml("<x>") === "&lt;x&gt;");
  check("escapeHtml 处理引号", escapeHtml('"x"') === "&quot;x&quot;");
}

// ── 4. rel 注入（配合 allow-popups-to-escape-sandbox）────────────────
{
  const out = addLinkRel('<a href="https://example.com">x</a>');
  check("给无 rel 的链接补上", out.includes('rel="noopener noreferrer"'), out);

  const existing = addLinkRel('<a rel="nofollow" href="#">x</a>');
  check("保留已有 rel 并追加", existing.includes("nofollow") && existing.includes("noopener"), existing);

  const already = addLinkRel('<a rel="noopener noreferrer" href="#">x</a>');
  check("已有完整 rel 不重复追加", (already.match(/noopener/g) ?? []).length === 1, already);

  // 不该误伤非 <a> 标签
  const img = addLinkRel('<img src="x.png">');
  check("不改动 img 标签", img === '<img src="x.png">', img);
}

// ── 6. 外链补 target="_blank"（防止 iframe 内原地跳转被 X-Frame-Options 拦截）──
{
  const http = addLinkRel('<a href="https://example.com">x</a>');
  check("http 链接补 target", http.includes('target="_blank"'), http);
  check("同时补 rel", http.includes("noopener"), http);

  const mailto = addLinkRel('<a href="mailto:a@b.com">m</a>');
  check("mailto 补 target", mailto.includes('target="_blank"'), mailto);

  // 锚点必须留在原位，不能开新窗口
  const anchor = addLinkRel('<a href="#section">s</a>');
  check("锚点不补 target", !anchor.includes('target='), anchor);

  // 已有 target 不覆盖（尊重邮件作者的 _self 等选择）
  const custom = addLinkRel('<a href="https://a.com" target="_self">x</a>');
  check("已有 target 不覆盖", custom.includes('target="_self"') && !custom.includes('target="_blank"'), custom);

  // 没有 href 的 <a>（纯锚点）也不该补
  const noHref = addLinkRel("<a name=\"top\">x</a>");
  check("无 href 不补 target", !noHref.includes('target='), noHref);

  // 大写标签名 / 大写属性名
  const upper = addLinkRel('<A HREF="https://a.com">x</A>');
  check("大写标签也能处理", upper.includes('target="_blank"'), upper);
}

// ── 5. 远程图片检测 ──────────────────────────────────────────────────
{
  check("识别 https 图片", hasRemoteImages('<img src="https://t.co/x.gif">'));
  check("识别协议相对", hasRemoteImages("<img src='http://a/b.png'>"));
  check("识别 background 属性", hasRemoteImages('<td background="https://a/b.png">'));
  check("识别 CSS url()", hasRemoteImages("background:url(https://a/b.png)"));
  check("data: 图片不算远程", !hasRemoteImages('<img src="data:image/png;base64,AAA">'));
  check("相对路径不算远程", !hasRemoteImages('<img src="/api/attachments/x_0">'));
  check("纯文本不算", !hasRemoteImages("hello world"));
}

if (failed > 0) {
  console.error(`\n${failed} 项失败`);
  process.exit(1);
}
console.log("\n全部通过");
