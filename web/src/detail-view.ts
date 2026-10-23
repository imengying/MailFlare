/**
 * 邮件详情渲染。
 *
 * 安全要点都在 `mail-render.ts` 里（sandbox iframe + CSP + 默认禁图），
 * 这里负责组装 DOM 与交互。
 */

import type { EmailDetail } from "./api-types.ts";
import { create, formatSize, isMobile } from "./dom.ts";
import { buildSrcdoc, hasRemoteImages, htmlToText } from "./mail-render.ts";
import type { AppState } from "./state.ts";
import { emptyState } from "./ui.ts";

export interface DetailHandlers {
  /** 切换 HTML / 纯文本。 */
  onTabChange(tab: "html" | "text"): void;
  /** 用户点了「显示图片」。 */
  onAllowImages(): void;
  /** 标为未读。 */
  onMarkUnread(id: string): void;
  /** 删除。 */
  onDelete(id: string): void;
  /** 复制 Message-ID（复制动作本身由调用方实现）。 */
  onCopyMessageId(messageId: string): void;
}

/** 详情加载中的骨架屏。 */
export function renderDetailLoading(): void {
  const pane = document.querySelector<HTMLElement>("#detail-pane");
  if (!pane) return;

  const head = create("div", { className: "detail-head" });
  head.append(
    create("div", { className: "skeleton", style: "height:20px;width:60%;margin-bottom:10px" }),
    create("div", { className: "skeleton", style: "height:12px;width:35%" }),
  );

  const body = create("div", { className: "body-wrap" });
  body.append(create("div", { className: "skeleton", style: "height:260px;margin-top:16px" }));

  pane.replaceChildren(head, body);
}

/** 详情加载失败。 */
export function renderDetailError(message: string): void {
  const pane = document.querySelector<HTMLElement>("#detail-pane");
  if (!pane) return;
  pane.replaceChildren(emptyState(message || "加载失败"));
}

/** 渲染详情头部（主题、元信息、操作按钮）。 */
export function renderDetail(state: AppState, handlers: DetailHandlers): void {
  const detail = state.detail;
  const pane = document.querySelector<HTMLElement>("#detail-pane");
  if (!detail || !pane) return;

  const head = create("div", { className: "detail-head" });
  head.append(create("h1", { className: "detail-subject", textContent: detail.subject || "(无主题)" }));

  const meta = create("div", { className: "meta-line" });
  const addMeta = (label: string, value: string): void => {
    if (!value) return;
    const span = create("span");
    span.append(create("b", { textContent: label }), document.createTextNode(` ${value}`));
    meta.append(span);
  };
  addMeta("发件人", detail.sender);
  addMeta("收件人", detail.recipient);
  addMeta("时间", new Date(detail.created_at).toLocaleString());
  head.append(meta);

  head.append(buildActions(detail, state.activeTab, handlers));
  pane.replaceChildren(head);
  renderDetailBody(state, handlers);
}

/** 操作按钮组。 */
function buildActions(
  detail: EmailDetail,
  activeTab: "html" | "text",
  handlers: DetailHandlers,
): HTMLElement {
  const actions = create("div", { className: "detail-actions" });

  if (detail.text_body || detail.html_body) {
    const htmlBtn = create("button", { className: "btn sm", type: "button", textContent: "HTML" });
    const textBtn = create("button", { className: "btn sm", type: "button", textContent: "纯文本" });

    const sync = (): void => {
      htmlBtn.classList.toggle("primary", activeTab === "html");
      textBtn.classList.toggle("primary", activeTab === "text");
    };
    sync();

    htmlBtn.addEventListener("click", () => handlers.onTabChange("html"));
    textBtn.addEventListener("click", () => handlers.onTabChange("text"));
    actions.append(htmlBtn, textBtn);
  }

  if (detail.raw_url) {
    const link = create("a", {
      className: "btn sm",
      href: detail.raw_url,
      textContent: "下载 .eml",
    });
    link.setAttribute("download", `${detail.id}.eml`);
    actions.append(link);
  }

  if (detail.message_id) {
    const messageId = detail.message_id;
    const copyBtn = create("button", {
      className: "btn sm ghost",
      type: "button",
      textContent: "复制 Message-ID",
    });
    copyBtn.addEventListener("click", () => handlers.onCopyMessageId(messageId));
    actions.append(copyBtn);
  }

  // 打开详情时后端已自动标为已读，所以这里提供「标回未读」的入口。
  const unreadBtn = create("button", {
    className: "btn sm",
    type: "button",
    textContent: "标为未读",
  });
  unreadBtn.addEventListener("click", () => handlers.onMarkUnread(detail.id));
  actions.append(unreadBtn);

  const deleteBtn = create("button", {
    className: "btn sm danger",
    type: "button",
    textContent: "删除",
  });
  deleteBtn.addEventListener("click", () => handlers.onDelete(detail.id));
  actions.append(deleteBtn);

  return actions;
}

/** 渲染正文与附件（可重复调用，用于切换 tab / 放开图片）。 */
export function renderDetailBody(state: AppState, handlers: DetailHandlers): void {
  const detail = state.detail;
  const pane = document.querySelector<HTMLElement>("#detail-pane");
  if (!detail || !pane) return;

  // 只替换 body-wrap，保留头部，避免切换 tab 时头部闪一下。
  pane.querySelectorAll(".body-wrap").forEach((node) => node.remove());

  const wrap = create("div", { className: "body-wrap" });
  wrap.append(buildBody(state, handlers));
  wrap.append(...buildAttachments(detail));
  pane.append(wrap);
}

/** 正文区域。 */
function buildBody(state: AppState, handlers: DetailHandlers): HTMLElement {
  const detail = state.detail;
  const box = create("div", { className: "body-scroll" });
  if (!detail) return box;

  if (state.activeTab === "html" && detail.html_body) {
    if (!state.allowRemoteImages && hasRemoteImages(detail.html_body)) {
      box.append(buildImageBar(handlers));
    }
    box.append(buildIframe(detail.html_body, state.allowRemoteImages));
    return box;
  }

  if (detail.text_body) {
    box.append(create("pre", { className: "text-body", textContent: detail.text_body }));
    return box;
  }

  if (detail.html_body) {
    // 没有纯文本正文时，从 HTML 提取文本。
    box.append(create("pre", { className: "text-body", textContent: htmlToText(detail.html_body) }));
    return box;
  }

  box.append(create("div", { className: "empty", textContent: "这封邮件没有正文" }));
  return box;
}

/** 「已阻止远程图片」提示条。 */
function buildImageBar(handlers: DetailHandlers): HTMLElement {
  const bar = create("div", { className: "imgbar" });
  bar.append(
    create("span", { textContent: "🛡️ 已阻止远程图片，避免向发件人泄露你的 IP" }),
  );
  const button = create("button", {
    className: "btn sm",
    type: "button",
    textContent: "显示图片",
  });
  button.addEventListener("click", () => handlers.onAllowImages());
  bar.append(button);
  return bar;
}

/** 沙箱 iframe。 */
function buildIframe(html: string, allowRemoteImages: boolean): HTMLIFrameElement {
  const iframe = create("iframe", { className: "mail-html" });
  // 关键：sandbox 里**不含** allow-scripts，脚本无法执行。
  // allow-same-origin 只用于读取 documentElement.scrollHeight 自适应高度。
  iframe.setAttribute("sandbox", "allow-same-origin");
  iframe.setAttribute("referrerpolicy", "no-referrer");
  iframe.srcdoc = buildSrcdoc(html, allowRemoteImages);

  iframe.addEventListener("load", () => {
    try {
      const doc = iframe.contentDocument;
      if (!doc) return;
      const height = Math.min(Math.max(doc.documentElement.scrollHeight, 320), 4000);
      iframe.style.height = `${height}px`;
    } catch {
      // 跨域文档读不到 scrollHeight，保持默认高度。
    }
  });

  return iframe;
}

/** 附件列表（不含内联资源）。 */
function buildAttachments(detail: EmailDetail): HTMLElement[] {
  const files = detail.attachments.filter((att) => !att.inline);
  if (files.length === 0) return [];

  const urlByIndex = new Map(detail.attachment_urls.map((entry) => [entry.index, entry.url]));
  const list = create("div", { className: "attachments" });

  for (const file of files) {
    const url = urlByIndex.get(file.index);
    const node = create("div", { className: `att${url ? "" : " missing"}` });
    const name = file.filename || "附件";

    if (url) {
      const link = create("a", { href: url });
      link.setAttribute("download", name);
      link.append(create("span", { className: "name", textContent: name }));
      node.append(link);
    } else {
      node.append(create("span", { className: "name", textContent: name }));
      node.append(create("span", { className: "size", textContent: "未保存" }));
    }
    node.append(create("span", { className: "size", textContent: formatSize(file.size) }));
    list.append(node);
  }

  return [list];
}

/** 移动端返回列表。 */
export function leaveDetail(): void {
  if (isMobile()) {
    document.querySelector("#app-view")?.classList.remove("showing");
  }
}
