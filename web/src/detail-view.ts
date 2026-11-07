/**
 * 邮件详情渲染。
 *
 * 安全要点都在 `mail-render.ts` 里（sandbox iframe + CSP + 默认禁图），
 * 这里负责组装 DOM 与交互。
 */

import type { EmailDetail } from "./api-types.ts";
import { create, formatSize } from "./dom.ts";
import { icon } from "./icons.ts";
import { buildSrcdoc, hasRemoteImages } from "./mail-render.ts";
import type { AppState } from "./state.ts";
import { senderInitial, senderTone } from "./sender.ts";
import { copyText, emptyState } from "./ui.ts";

export interface DetailHandlers {
  onBack(): void;
  /** 用户点了「显示图片」。 */
  onAllowImages(): void;
  /** 标为未读。 */
  onMarkUnread(id: string): void;
  /** 删除。 */
  onDelete(id: string): void;
  onRestore(id: string): void;
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
  head.append(create("h1", { className: "detail-subject", textContent: detail.subject || "(无主题)" }));
  if (detail.otp_code !== null) head.append(buildOtp(detail.otp_code));

  const meta = create("dl", { className: "meta-line" });
  const addMeta = (label: string, value: string): HTMLElement | undefined => {
    if (!value) return;
    const entry = create("dd", { textContent: value });
    meta.append(create("dt", { textContent: label }), entry);
    return entry;
  };
  addMeta("发件人", detail.sender);
  const recipient = addMeta("收件人", detail.recipient);
  if (recipient) {
    recipient.classList.add("recipient-value");
    recipient.replaceChildren(create("span", { textContent: detail.recipient }));
    const copy = create("button", { className: "btn ghost icon-btn sm copy-address", type: "button", title: "复制收件地址", ariaLabel: "复制收件地址" });
    copy.append(icon("copy"));
    copy.addEventListener("click", () => {
      void copyText(detail.recipient, "收件地址已复制");
    });
    recipient.append(copy);
  }
  const metaWrap = create("div", { className: "detail-meta" });
  const info = create("div", { className: "detail-info" });
  metaWrap.append(
    create("span", { className: `mail-avatar detail-avatar tone-${senderTone(detail.sender)}`, textContent: senderInitial(detail.sender), ariaHidden: "true" }),
    info,
  );
  const timeRow = create("div", { className: "meta-time" });
  timeRow.append(
    create("span", {
      textContent: new Date(detail.created_at).toLocaleString("zh-CN", { hour12: false }),
    }),
  );
  // 推送状态紧跟在时间后面：有验证码的邮件才由后端写入，其余为 null 不显示。
  if (detail.push_status) {
    const ok = detail.push_status === "ok";
    timeRow.append(
      create("span", {
        className: `push-badge ${ok ? "ok" : "failed"}`,
        textContent: ok ? "已推送" : detail.push_status === "pending" ? "等待推送重试" : "推送失败",
        title: ok ? "验证码已发送到通知渠道" : detail.push_status === "pending" ? "系统将限时重试失败的渠道" : "验证码推送未完成，可在设置页测试通知渠道",
      }),
    );
  }
  info.append(meta, timeRow);
  head.append(metaWrap);
  if (detail.storage_status !== "complete") {
    const notice = create("div", { className: "notice", id: "storage-notice", role: "status" });
    let message = "彻底删除尚未完成，期间无法恢复；请再次点击「彻底删除」继续清理。";
    if (detail.storage_status === "pending") {
      message = "部分附件暂未保存，系统正在重试。";
      if (detail.original_url) message += "可先通过「导出原文」查看完整内容。";
      if (detail.deleted_at !== null) message += "等待附件恢复完成后，才能彻底删除。";
    }
    notice.append(icon("alert"), create("span", { textContent: message }));
    head.append(notice);
  }

  pane.replaceChildren(buildActions(detail, handlers), head);
  renderDetailBody(state, handlers);
}

function buildOtp(code: string): HTMLElement {
  const bar = create("div", { className: "otp-bar", ariaLabel: "邮件验证码" });
  const value = create("span", { className: "otp-code", textContent: code });
  const copy = create("button", { className: "btn sm", type: "button", ariaLabel: "复制验证码" });
  const label = create("span", { textContent: "复制" });
  copy.append(icon("copy"), label);
  let feedback: ReturnType<typeof setTimeout> | undefined;
  copy.addEventListener("click", () => {
    void copyText(code, "验证码已复制").then(ok => {
      if (!ok || !copy.isConnected) return;
      clearTimeout(feedback);
      copy.replaceChildren(icon("check"), label);
      feedback = setTimeout(() => copy.replaceChildren(icon("copy"), label), 1600);
    });
  });
  bar.append(create("span", { className: "otp-label", textContent: "验证码" }), value, copy);
  return bar;
}

/** 操作按钮组。 */
function buildActions(detail: EmailDetail, handlers: DetailHandlers): HTMLElement {
  const toolbar = create("div", { className: "detail-toolbar", role: "group", ariaLabel: "邮件操作" });
  const actions = create("div", { className: "detail-actions" });

  const back = create("button", { className: "btn sm ghost detail-back", type: "button", textContent: "返回" });
  back.prepend(icon("back"));
  back.addEventListener("click", handlers.onBack);
  toolbar.append(back, actions);

  const deleteLabel = detail.deleted_at !== null ? "彻底删除" : "删除";
  const deleteBtn = create("button", { className: `btn sm ghost danger${detail.deleted_at === null ? " compact-action" : ""}`, type: "button", ariaLabel: deleteLabel, title: deleteLabel });
  deleteBtn.append(icon("trash"), create("span", { className: "action-label", textContent: deleteLabel }));
  deleteBtn.addEventListener("click", () => handlers.onDelete(detail.id));
  if (detail.deleted_at !== null) {
    const restore = create("button", { className: "btn sm ghost", type: "button", textContent: "恢复" });
    restore.prepend(icon("restore"));
    restore.disabled = detail.storage_status === "purging";
    if (restore.disabled) restore.setAttribute("aria-describedby", "storage-notice");
    deleteBtn.disabled = detail.storage_status === "pending";
    if (deleteBtn.disabled) deleteBtn.setAttribute("aria-describedby", "storage-notice");
    restore.addEventListener("click", () => handlers.onRestore(detail.id));
    actions.append(restore);
  } else {
    const unreadBtn = create("button", { className: "btn sm ghost compact-action", type: "button", ariaLabel: "标为未读", title: "标为未读" });
    unreadBtn.append(icon("mail"), create("span", { className: "action-label", textContent: "标为未读" }));
    unreadBtn.addEventListener("click", () => handlers.onMarkUnread(detail.id));
    actions.append(unreadBtn);
  }
  if (detail.original_url) {
    const download = create("a", { className: "btn sm ghost compact-action", ariaLabel: "导出原文", title: "导出原文", href: detail.original_url });
    download.append(icon("download"), create("span", { className: "action-label", textContent: "导出原文" }));
    download.setAttribute("download", `${detail.id}.eml`);
    actions.append(download);
  }
  actions.append(create("span", { className: "action-divider", ariaHidden: "true" }), deleteBtn);
  return toolbar;
}

/** 渲染正文与附件（可重复调用，用于放开图片后重绘）。 */
export function renderDetailBody(state: AppState, handlers: DetailHandlers): void {
  const detail = state.detail;
  const pane = document.querySelector<HTMLElement>("#detail-pane");
  if (!detail || !pane) return;

  // 只替换正文，保留头部，避免放开图片时头部闪烁。
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

  // 默认就是 HTML；没有 HTML 的邮件（纯文本信、解析失败）才退回纯文本。
  if (detail.html_body) {
    if (!state.allowRemoteImages && hasRemoteImages(detail.html_body)) {
      box.append(buildImageBar(handlers));
    }
    const frame = create("div", { className: "mail-frame" });
    frame.append(buildIframe(detail.html_body, state.allowRemoteImages));
    box.append(frame);
    return box;
  }

  if (detail.text_body) {
    box.append(create("pre", { className: "text-body", textContent: detail.text_body }));
    return box;
  }

  box.append(emptyState("没有正文", "这封邮件只包含附件或正文为空"));
  return box;
}

/** 「已阻止远程图片」提示条。 */
function buildImageBar(handlers: DetailHandlers): HTMLElement {
  const bar = create("div", { className: "imgbar" });
  const text = create("span", { textContent: "已阻止远程图片，避免泄露你的 IP" });
  text.prepend(icon("shield"));
  bar.append(text);
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
  const iframe = create("iframe", { className: "mail-html", title: "邮件正文" });
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
      create("span", { className: "size", textContent: formatSize(file.size) }),
    );
    node.append(icon("paperclip"));
    if (url) {
      const link = create("a", { href: url, title: `下载 ${name}` });
      link.setAttribute("download", name);
      link.append(info);
      node.append(link);
    } else {
      const explanation = detail.original_url && detail.storage_status !== "purging"
        ? "暂时无法单独下载，可通过「导出原文」查看附件。"
        : "暂无可下载文件。";
      info.append(
        create("span", { className: "attachment-status", textContent: "未保存", title: explanation }),
        create("span", { className: "attachment-help", textContent: explanation }),
      );
      node.append(info);
    }
    list.append(node);
  }
  return [section];
}
