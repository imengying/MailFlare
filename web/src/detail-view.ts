/**
 * 邮件详情渲染。
 *
 * 安全要点都在 `mail-render.ts` 里（sandbox iframe + CSP + 默认禁图），
 * 这里负责组装 DOM 与交互。
 */

import type { EmailDetail } from "./api-types.ts";
import { create, formatSize, isMobile } from "./dom.ts";
import { icon } from "./icons.ts";
import { buildSrcdoc, hasRemoteImages, htmlToText } from "./mail-render.ts";
import type { AppState } from "./state.ts";
import { senderInitial, senderTone } from "./sender.ts";
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
  /** 复制验证码（复制动作本身由调用方实现）。 */
  onCopyOtp(code: string): void;
}

/** 详情加载中的骨架屏。 */
export function renderDetailLoading(): void {
  const pane = document.querySelector<HTMLElement>("#detail-pane");
  if (!pane) return;

  const head = create("div", { className: "detail-head" });
  head.append(
    create("div", { className: "skeleton", style: "height:23px;width:60%;margin-bottom:20px" }),
    create("div", { className: "skeleton", style: "height:12px;width:75%;margin-bottom:9px" }),
    create("div", { className: "skeleton", style: "height:12px;width:55%" }),
  );

  const body = create("div", { className: "body-wrap" });
  body.append(create("div", { className: "skeleton", style: "height:260px;margin-top:16px" }));

  pane.replaceChildren(head, body);
}

/** 详情加载失败。 */
export function renderDetailError(message: string): void {
  const pane = document.querySelector<HTMLElement>("#detail-pane");
  if (!pane) return;
  pane.replaceChildren(emptyState("邮件加载失败", message || "请稍后重试"));
}

/** 渲染详情头部（主题、元信息、操作按钮）。 */
export function renderDetail(state: AppState, handlers: DetailHandlers): void {
  const detail = state.detail;
  const pane = document.querySelector<HTMLElement>("#detail-pane");
  if (!detail || !pane) return;

  const head = create("div", { className: "detail-head" });
  const top = create("div", { className: "detail-top" });
  top.append(
    create("h1", { className: "detail-subject", textContent: detail.subject || "(无主题)" }),
    buildActions(detail, handlers),
  );
  head.append(top);

  // 有验证码就把它提到标题下面：用户多半只为取这一串数字点开邮件。
  if (detail.otp_code) {
    const pill = create("button", {
      className: "otp-pill",
      type: "button",
      title: "点击复制验证码",
    });
    pill.append(
      icon("code"),
      create("span", { className: "otp-label", textContent: "验证码" }),
      create("span", { className: "otp-value", textContent: detail.otp_code }),
      create("span", { className: "otp-copy", textContent: "复制" }),
    );
    pill.addEventListener("click", () => handlers.onCopyOtp(detail.otp_code!));
    head.append(pill);
  }

  const meta = create("dl", { className: "meta-line" });
  const addMeta = (label: string, value: string): void => {
    if (!value) return;
    meta.append(create("dt", { textContent: label }), create("dd", { textContent: value }));
  };
  addMeta("发件人", detail.sender);
  addMeta("收件人", detail.recipient);
  const metaWrap = create("div", { className: "detail-meta" });
  const info = create("div", { className: "detail-info" });
  metaWrap.append(
    create("span", { className: `mail-avatar detail-avatar tone-${senderTone(detail.sender)}`, textContent: senderInitial(detail.sender), ariaHidden: "true" }),
    info,
  );
  info.append(meta, create("div", {
    className: "meta-time",
    textContent: new Date(detail.created_at).toLocaleString("zh-CN", { hour12: false }),
  }));
  head.append(metaWrap);

  pane.replaceChildren(head);
  renderDetailBody(state, handlers);
}

/** 操作按钮组。 */
function buildActions(detail: EmailDetail, handlers: DetailHandlers): HTMLElement {
  const actions = create("div", { className: "detail-actions" });

  const back = create("button", { className: "btn sm ghost detail-back", type: "button", textContent: "返回" });
  back.prepend(icon("back"));
  back.addEventListener("click", leaveDetail);
  actions.append(back);

  const unreadBtn = create("button", { className: "btn sm", type: "button", textContent: "标为未读", title: "标为未读" });
  unreadBtn.prepend(icon("mail"));
  unreadBtn.addEventListener("click", () => handlers.onMarkUnread(detail.id));

  const deleteBtn = create("button", { className: "btn sm danger", type: "button", textContent: "删除", title: "删除邮件" });
  deleteBtn.prepend(icon("trash"));
  deleteBtn.addEventListener("click", () => handlers.onDelete(detail.id));
  actions.append(unreadBtn, deleteBtn);
  return actions;
}

/** 渲染正文与附件（可重复调用，用于切换 tab / 放开图片）。 */
export function renderDetailBody(state: AppState, handlers: DetailHandlers): void {
  const detail = state.detail;
  const pane = document.querySelector<HTMLElement>("#detail-pane");
  if (!detail || !pane) return;

  // 只替换 body-wrap，保留头部，避免切换 tab 时头部闪一下。
  pane.querySelectorAll(".body-wrap").forEach((node) => node.remove());

  // 只有纯文本时，选中态也必须落在纯文本上。
  const activeTab = detail.html_body ? state.activeTab : "text";
  const wrap = create("div", { className: "body-wrap" });
  wrap.append(buildBodyToolbar(detail, activeTab, handlers));
  wrap.append(buildBody(state, handlers));
  wrap.append(...buildAttachments(detail));
  pane.append(wrap);
}

/** 正文格式切换与辅助操作，和邮件操作分开，减少头部按钮数量。 */
function buildBodyToolbar(
  detail: EmailDetail,
  activeTab: "html" | "text",
  handlers: DetailHandlers,
): HTMLElement {
  const bar = create("div", { className: "body-toolbar" });
  if (detail.text_body || detail.html_body) {
    const tabs = create("div", { className: "tabs", role: "tablist", ariaLabel: "正文格式" });
    for (const [tab, label, iconName] of [["html", "HTML", "code"], ["text", "纯文本", "type"]] as const) {
      const button = create("button", {
        className: `tab${activeTab === tab ? " active" : ""}`,
        type: "button",
        role: "tab",
      });
      button.setAttribute("aria-selected", String(activeTab === tab));
      button.textContent = label;
      button.prepend(icon(iconName));
      button.addEventListener("click", () => {
        if (activeTab !== tab) handlers.onTabChange(tab);
      });
      tabs.append(button);
    }
    bar.append(tabs);
  }
  if (detail.message_id) {
    const messageId = detail.message_id;
    const copy = create("button", { className: "btn sm ghost", type: "button", textContent: "复制 Message-ID", title: messageId });
    copy.prepend(icon("copy"));
    copy.addEventListener("click", () => handlers.onCopyMessageId(messageId));
    bar.append(copy);
  }
  return bar;
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

  box.append(emptyState("没有正文", "这封邮件只包含附件或正文为空"));
  return box;
}

/** 「已阻止远程图片」提示条。 */
function buildImageBar(handlers: DetailHandlers): HTMLElement {
  const bar = create("div", { className: "imgbar" });
  bar.append(
    (() => {
      const text = create("span", { textContent: "已阻止远程图片，避免泄露你的 IP" });
      text.prepend(icon("shield"));
      return text;
    })(),
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

/** iframe 最大高度；超过这个高度就放开内部滚动，避免生成几万像素高的元素。 */
const MAX_IFRAME_HEIGHT = 12000;

/** 沙箱 iframe。 */
function buildIframe(html: string, allowRemoteImages: boolean): HTMLIFrameElement {
  const iframe = create("iframe", { className: "mail-html" });
  // sandbox 取值说明（每一项都是必需的）：
  // - allow-same-origin：只用于读 documentElement.scrollHeight 做自适应高度。
  //   单独存在时**不会**授予脚本能力（无 allow-scripts），也无法访问父页面。
  // - allow-popups + allow-popups-to-escape-sandbox：邮件里的 <a target="_blank">
  //   才能在新标签页打开。缺了这两个，链接点了没反应——这是邮件客户端的基本预期。
  // - **不含 allow-scripts**：脚本一律不执行。这是防 XSS 的核心。
  // - **不含 allow-top-navigation**：防止邮件把整个页面导航走。
  // - **不含 allow-forms**：防止钓鱼表单提交（另有 CSP 的 form-action 'none' 兜底）。
  iframe.setAttribute(
    "sandbox",
    "allow-same-origin allow-popups allow-popups-to-escape-sandbox",
  );
  iframe.setAttribute("referrerpolicy", "no-referrer");
  iframe.srcdoc = buildSrcdoc(html, allowRemoteImages);

  iframe.addEventListener("load", () => {
    fitIframe(iframe);

    // 图片是异步加载的，加载完正文会变高；只在 load 时算一次会把底部截断。
    try {
      const doc = iframe.contentDocument;
      if (!doc) return;
      for (const img of doc.querySelectorAll("img")) {
        if (img.complete) continue;
        img.addEventListener("load", () => fitIframe(iframe));
        img.addEventListener("error", () => fitIframe(iframe));
      }
    } catch {
      // 读不到文档就退化成固定高度。
    }

    // 字体替换等布局变化没有事件可听，补两次延迟重算。
    setTimeout(() => fitIframe(iframe), 400);
    setTimeout(() => fitIframe(iframe), 1200);
  });

  return iframe;
}

/**
 * 按正文实际高度调整 iframe，避免「外层 + iframe 内部」两条滚动条。
 *
 * - 正文不超高：iframe 撑到完整高度并关掉内部滚动，只留 `.detail-pane` 一条。
 * - 正文超长：iframe 定高，此时必须放开内部滚动，否则尾部内容永远读不到；
 *   这是超长邮件的无奈之选，普通邮件走不到这个分支。
 */
function fitIframe(iframe: HTMLIFrameElement): void {
  try {
    const doc = iframe.contentDocument;
    if (!doc) return;
    const full = Math.max(doc.documentElement.scrollHeight, 320);
    if (full > MAX_IFRAME_HEIGHT) {
      iframe.style.height = `${MAX_IFRAME_HEIGHT}px`;
      iframe.setAttribute("scrolling", "yes");
    } else {
      iframe.style.height = `${full}px`;
      iframe.setAttribute("scrolling", "no");
    }
  } catch {
    // 跨域文档读不到 scrollHeight，保持默认高度。
  }
}

/** 附件列表（不含内联资源）。 */
function buildAttachments(detail: EmailDetail): HTMLElement[] {
  const files = detail.attachments.filter((att) => !att.inline);
  if (files.length === 0) return [];

  const urlByIndex = new Map(detail.attachment_urls.map((entry) => [entry.index, entry.url]));
  const section = create("section", { className: "attachments", ariaLabel: "邮件附件" });
  const heading = create("h2", { className: "section-label", textContent: `附件 · ${files.length}` });
  heading.prepend(icon("paperclip"));
  const list = create("div", { className: "attachment-list" });
  section.append(heading, list);

  for (const file of files) {
    const url = urlByIndex.get(file.index);
    const node = create("div", { className: `att${url ? "" : " missing"}` });
    const name = file.filename || "附件";
    const info = create("div", { className: "att-info" });
    info.append(
      create("span", { className: "name", textContent: name, title: name }),
      create("span", { className: "size", textContent: `${formatSize(file.size)}${url ? "" : " · 未保存"}` }),
    );
    node.append(icon("paperclip"));
    if (url) {
      const link = create("a", { href: url, title: `下载 ${name}` });
      link.setAttribute("download", name);
      link.append(info);
      node.append(link);
    } else {
      node.append(info);
    }
    list.append(node);
  }
  return [section];
}

/** 移动端返回列表。 */
export function leaveDetail(): void {
  if (isMobile()) {
    document.querySelector("#app-view")?.classList.remove("showing");
  }
}
