/**
 * 邮件 HTML 正文的渲染安全层。
 *
 * 这是整个前端最需要谨慎的地方：邮件正文是**完全不可信**的第三方 HTML。
 *
 * 三道防线，任何一道单独失效都还能兜住：
 * 1. 渲染在 `sandbox="allow-same-origin"` 的 iframe 里，**不含**
 *    `allow-scripts` —— 脚本无法执行，也无法触碰父页面。
 * 2. `srcdoc` 里内嵌 CSP（`default-src 'none'`），即使沙箱属性被某个浏览器
 *    忽略，脚本/表单/外部请求依然被拦。
 * 3. 默认拦截远程图片（`img-src 'self' data:`），避免追踪像素泄露 IP；
 *    用户显式点「显示图片」后才放开。
 */

/** 判断正文里是否存在需要联网加载的图片（用于显示「显示图片」提示条）。 */
export function hasRemoteImages(html: string): boolean {
  return (
    /<img[^>]+src\s*=\s*["']?https?:/i.test(html) ||
    /background\s*=\s*["']?https?:/i.test(html) ||
    /url\(\s*["']?https?:/i.test(html)
  );
}

/**
 * 构造 iframe 的 `srcdoc`。
 *
 * 处理三种真实世界的邮件结构：
 * - 完整文档（有 `<head>`）：把注入内容插进 head
 * - 只有 `<html>` 包裹：补一个 head
 * - 裸片段（没有 html/body）：整段前置一个 head
 *
 * 另外，若「声明是 HTML、内容其实是纯文本」，补一层 `<pre>` 保留换行。
 */
export function buildSrcdoc(html: string, allowRemoteImages: boolean): string {
  const imgSrc = allowRemoteImages ? "img-src * data: blob:" : "img-src 'self' data:";
  // 注意不要加 `base-uri 'none'`：那会连下面的 <base target="_blank"> 一起禁用，
  // 导致邮件里的链接点不动。
  const csp =
    `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; ` +
    `${imgSrc}; style-src 'unsafe-inline'; font-src data:; form-action 'none'">`;

  const head =
    '<base target="_blank">' +
    '<meta name="referrer" content="no-referrer">' +
    csp +
    "<style>" +
    "html,body{margin:0;padding:0}" +
    'body{font:14px/1.7 -apple-system,system-ui,"PingFang SC",sans-serif;word-break:break-word}' +
    "img{max-width:100%;height:auto}" +
    "table{max-width:100%}" +
    "a{color:#2f6bff}" +
    "</style>";

  const looksLikeHtml = /<\/?(html|body|div|p|br|table|span|a|img|ul|ol|li|h[1-6])\b/i.test(html);
  if (!looksLikeHtml) {
    return `${head}<pre style="white-space:pre-wrap;word-break:break-word;margin:0">${escapeHtml(html)}</pre>`;
  }

  // `allow-popups-to-escape-sandbox` 让新标签页脱离沙箱，所以必须确保它们
  // 拿不到 opener 句柄。CSP 无法表达 rel，只能在字符串层面补。
  const guarded = addLinkRel(html);

  if (/<head[\s>]/i.test(guarded)) {
    return guarded.replace(/<head([^>]*)>/i, `<head$1>${head}`);
  }
  if (/<html[\s>]/i.test(guarded)) {
    return guarded.replace(/<html([^>]*)>/i, `<html$1><head>${head}</head>`);
  }
  return `<head>${head}</head>${guarded}`;
}

/**
 * 给所有 `<a>` 补上 `rel="noopener noreferrer"`。
 *
 * 配合 `allow-popups-to-escape-sandbox`：新标签页虽然脱离了沙箱，
 * 但拿不到 `window.opener`，无法反向操作本页面，也带不上 Referer。
 *
 * 只做字符串替换而不是 DOM 解析，因为 srcdoc 是纯文本、且不能依赖
 * iframe 内部的脚本执行。
 */
export function addLinkRel(html: string): string {
  return html.replace(/<a\b([^>]*)>/gi, (_match, attrs: string) => {
    let next = attrs;

    // ── rel：补齐 noopener / noreferrer ──────────────────────────────
    const existingRel = /\brel\s*=\s*(["'])(.*?)\1/i.exec(next);
    if (existingRel) {
      const current = existingRel[2] ?? "";
      const lower = current.toLowerCase();
      const needs = ["noopener", "noreferrer"].filter((token) => !lower.includes(token));
      if (needs.length > 0) {
        const merged = current.trim() ? `${current} ${needs.join(" ")}` : needs.join(" ");
        next = next.replace(existingRel[0], `rel="${merged}"`);
      }
    } else {
      next += ' rel="noopener noreferrer"';
    }

    // ── target：外链必须在新标签打开 ─────────────────────────────────
    // 没有 target 的链接会在 iframe 内原地跳转，被目标站的
    // `X-Frame-Options: DENY` 拒绝，用户看到一片空白。
    // 锚点（`#` 开头）要留在原位：它指向文档内部，不能开新窗口。
    const href = /\bhref\s*=\s*(["'])(.*?)\1/i.exec(next)?.[2]?.trim() ?? "";
    const isAnchor = href === "" || href.startsWith("#");
    if (!isAnchor && !/\btarget\s*=/i.test(next)) {
      next += ' target="_blank"';
    }

    return `<a${next}>`;
  });
}

/**
 * HTML 正文取纯文本（用于「纯文本」视图的兜底）。
 *
 * 用 `DOMParser` 而不是 `div.innerHTML = ...`：DOMParser 产出的文档是惰性的，
 * 解析过程中不会执行脚本，也不会发起图片/样式请求。
 */
export function htmlToText(html: string): string {
  const doc = new DOMParser().parseFromString(html, "text/html");
  return doc.body.textContent ?? "";
}

/** 最小 HTML 转义。 */
export function escapeHtml(input: string): string {
  return input
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}
