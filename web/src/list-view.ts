/**
 * 邮件列表渲染。
 *
 * 纯渲染函数：只读 `AppState`，不改它。所有交互通过回调注入，
 * 这样渲染逻辑可以独立测试，也不会和网络层纠缠。
 */

import type { EmailSummary } from "./api-types.ts";
import { create, el, formatTime } from "./dom.ts";
import { icon } from "./icons.ts";
import type { AppState } from "./state.ts";
import { senderInitial, senderLabel, senderTone } from "./sender.ts";
import { emptyState, skeletonRows } from "./ui.ts";

export interface ListHandlers {
  /** 点击某封邮件。 */
  onOpen(id: string): void;
  /** 加载下一页。 */
  onLoadMore(): void;
}

/** 更新侧边栏「未读邮件」角标。 */
function renderUnreadBadge(state: AppState): void {
  const badge = el<HTMLElement>("#count-unread-nav");
  if (state.unread > 0) {
    badge.hidden = false;
    badge.textContent = state.unread > 99 ? "99+" : String(state.unread);
  } else {
    badge.hidden = true;
    badge.textContent = "";
  }
}

/** 渲染整个列表。 */
export function renderList(state: AppState, handlers: ListHandlers): void {
  const pane = el<HTMLElement>("#list-pane");
  renderUnreadBadge(state);
  const count = el<HTMLElement>("#list-count");
  count.hidden = state.items.length === 0;
  // 搜索已由后端过滤，显示当前已加载的邮件数量。
  count.textContent = `${state.items.length} 封`;
  const allRead = el<HTMLButtonElement>("#mark-all-read");
  allRead.disabled = state.unread === 0 || state.filter === "trash";
  allRead.title = state.filter === "trash" ? "回收站不参与全部已读操作" : state.unread === 0 ? "没有未读邮件" : `将 ${state.unread} 封未读邮件标为已读`;

  if (state.loadingList) {
    pane.replaceChildren(...skeletonRows(6));
    return;
  }

  // 切换筛选时保留旧列表，只降低透明度表示「正在换」。
  // 直接清空再填会闪一下骨架屏，观感像整页刷新。
  pane.classList.toggle("is-switching", state.switching);

  const items = state.items;
  if (items.length === 0) {
    pane.replaceChildren(
      state.search
        ? emptyState("没有匹配的邮件", "换个关键词，或清空搜索查看全部邮件", "search")
        : state.filter === "trash" ? emptyState("回收站是空的", "删除的邮件会保留在这里，可恢复或彻底删除", "trash")
        : emptyState("收件箱是空的", state.filter === "unread" ? "所有邮件都已读完" : "新邮件到达后会显示在这里", "inbox"),
    );
    return;
  }

  const frag = document.createDocumentFragment();
  for (const item of items) {
    frag.append(buildRow(item, state.selectedId === item.id, handlers.onOpen));
  }

  // 「加载更多」而不是无限滚动：行为可预测，也便于整页可见性判断。
  if (state.nextCursor !== null) {
    const more = create("button", {
      className: "btn ghost load-more",
      textContent: state.loadingMore ? "加载中…" : "加载更多",
    });
    more.disabled = state.loadingMore;
    more.type = "button";
    more.addEventListener("click", () => handlers.onLoadMore());
    frag.append(more);
  }

  pane.replaceChildren(frag);
}

/** 单行邮件。 */
function buildRow(item: EmailSummary, selected: boolean, onOpen: (id: string) => void): HTMLElement {
  const row = create("button", {
    className: `mail-item${item.is_read ? "" : " unread"}${selected ? " selected" : ""}`,
    type: "button",
    ariaLabel: `${item.is_read ? "" : "未读，"}${item.sender || "未知发件人"}，${item.subject || "无主题"}${item.recipient ? `，收件地址：${item.recipient}` : ""}`,
  });
  if (selected) row.setAttribute("aria-current", "true");

  const name = senderLabel(item.sender);
  row.append(create("span", {
    className: `mail-avatar tone-${senderTone(item.sender)}`,
    textContent: senderInitial(item.sender),
    ariaHidden: "true",
  }));
  const content = create("div", { className: "mail-content" });
  const head = create("div", { className: "row1" });
  head.append(
    create("span", { className: "unread-dot" }),
    create("span", {
      className: "from",
      textContent: name,
      title: item.sender,
    }),
    create("span", { className: "when", textContent: formatTime(item.created_at), title: new Date(item.created_at).toLocaleString("zh-CN", { hour12: false }) }),
  );
  content.append(head);

  content.append(create("div", { className: "subj", textContent: item.subject || "(无主题)", title: item.subject }));

  // 压缩空白后截断；`textContent` 赋值保证不会把预览当 HTML 解析。
  content.append(create("div", {
    className: "snippet",
    textContent: item.preview.replace(/\s+/g, " ").trim().slice(0, 120) || "暂无正文预览",
  }));
  // 收件地址独占元信息行，避免被长摘要挤到只剩标签；域名也保留以区分多域邮箱。
  const footer = create("div", { className: "mail-footer" });
  if (item.recipient) {
    const to = create("span", { className: "mail-recipient", title: `收件地址：${item.recipient}` });
    to.append(
      create("span", { className: "recipient-label", textContent: "收件" }),
      create("span", { className: "recipient-address", textContent: item.recipient }),
    );
    footer.append(to);
  }
  if (item.attachment_count > 0) {
    const tag = create("span", { className: "attachment-tag", title: `${item.attachment_count} 个附件` });
    tag.append(icon("paperclip"), document.createTextNode(String(item.attachment_count)));
    footer.append(tag);
  }
  if (footer.childElementCount > 0) content.append(footer);
  row.append(content);
  row.addEventListener("click", () => onOpen(item.id));
  return row;
}
