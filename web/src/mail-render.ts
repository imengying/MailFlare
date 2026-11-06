/** 邮件 HTML 只在惰性 template 中解析，随后进入无脚本的 sandbox iframe。 */
function parse(html: string): HTMLTemplateElement {
  const template = document.createElement("template");
  template.innerHTML = html;
  return template;
}

export function hasRemoteImages(html: string): boolean {
  const template = parse(html);
  const remote = /(?:https?:)?\/\//i;
  return [...template.content.querySelectorAll("[src], [srcset], [background]")].some((node) =>
    ["src", "srcset", "background"].some((attr) => remote.test(node.getAttribute(attr) ?? "")),
  ) || /url\(\s*["']?(?:https?:)?\/\//i.test(html);
}

/** 真实属性解析处理无引号/实体/重复属性；禁止改变父页面或复用已命名窗口。 */
function guardLinks(template: HTMLTemplateElement): void {
  for (const link of template.content.querySelectorAll("a, area")) {
    const href = (link.getAttribute("href") ?? "").trim();
    const rel = new Set((link.getAttribute("rel") ?? "").toLowerCase().split(/\s+/).filter(Boolean));
    rel.delete("opener");
    rel.add("noopener");
    rel.add("noreferrer");
    link.setAttribute("rel", [...rel].join(" "));
    link.removeAttribute("ping");
    link.removeAttribute("target");
    if (!href || href.startsWith("#")) continue;
    if (/^(https?:\/\/|mailto:)/i.test(href)) link.setAttribute("target", "_blank");
    else link.removeAttribute("href");
  }
}

export function buildSrcdoc(html: string, allowRemoteImages: boolean): string {
  const template = parse(html);
  // 禁止独立文档、导航、脚本、嵌套模板与有副作用的表单/资源预加载。
  template.content.querySelectorAll("script, iframe, frame, frameset, object, embed, meta, base, link, template, form, svg, math").forEach((node) => node.remove());
  for (const node of template.content.querySelectorAll("*")) {
    for (const attr of [...node.attributes]) {
      if (/^on/i.test(attr.name) || ["srcdoc", "autofocus", "formaction"].includes(attr.name)) node.removeAttribute(attr.name);
    }
  }
  guardLinks(template);
  const imgSrc = allowRemoteImages ? "img-src https: http: data:" : `img-src ${location.origin}/api/attachments/ data:`;
  // 可信前缀先建立 CSP；邮件里的注释、畸形 head 和解析状态都无法影响它。
  return '<!doctype html><html><head><meta charset="utf-8">' +
    `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; ${imgSrc}; style-src 'unsafe-inline'; font-src data:; form-action 'none'; base-uri 'none'">` +
    '<meta name="referrer" content="no-referrer">' +
    '<style>html,body{margin:0;padding:0}body{font:14px/1.7 -apple-system,system-ui,"PingFang SC",sans-serif;word-break:break-word;color:#1f2329;background:#fff;padding:16px 20px;color-scheme:light}img{max-width:100%;height:auto}table{max-width:100%}a{color:#2f6bff}</style>' +
    '</head><body>' + (/</.test(html) ? template.innerHTML : `<pre style="white-space:pre-wrap">${escapeHtml(html)}</pre>`) + '</body></html>';
}

function escapeHtml(input: string): string {
  return input.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}
