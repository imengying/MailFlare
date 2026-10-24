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

/** 按本地搜索关键词过滤（纯前端，不触发网络请求）。 */
export function visibleItems(state: AppState): EmailSummary[] {
  if (!state.search) return state.items;
  const query = state.search.toLowerCase();
  return state.items.filter(
    (item) =>
      item.subject.toLowerCase().includes(query) ||
      item.sender.toLowerCase().includes(query) ||
      item.preview.toLowerCase().includes(query),
  );
}

/** 更新侧边栏「未读邮件」角标。标为已读时单独调用，不必重绘整个列表。 */
export function renderUnreadBadge(state: AppState): void {
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
  count.textContent = state.search
    ? `${visibleItems(state).length} / ${state.items.length} 封`
    : `${state.items.length} 封`;
  const allRead = el<HTMLButtonElement>("#mark-all-read");
  allRead.disabled = state.unread === 0;
  allRead.title = state.unread === 0 ? "没有未读邮件" : `将 ${state.unread} 封未读邮件标为已读`;

  if (state.loadingList) {
    pane.replaceChildren(...skeletonRows(6));
    return;
  }

  const items = visibleItems(state);
  if (items.length === 0) {
    pane.replaceChildren(
      state.search
        ? emptyState("没有匹配的邮件", "换个关键词，或清空搜索查看全部邮件", "search")
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

/** 取收件地址 `@` 之前的部分；没有别名可省时返回空串。 */
function recipientAlias(recipient: string): string {
  const at = recipient.lastIndexOf("@");
  if (at <= 0) return "";
  return recipient.slice(0, at);
}

/** 单行邮件。 */
function buildRow(item: EmailSummary, selected: boolean, onOpen: (id: string) => void): HTMLElement {
  const row = create("button", {
    className: `mail-item${item.is_read ? "" : " unread"}${selected ? " selected" : ""}`,
    type: "button",
    ariaLabel: `${item.is_read ? "" : "未读，"}${item.sender || "未知发件人"}，${item.subject || "无主题"}`,
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
    create("span", { className: "when", textContent: formatTime(item.created_at) }),
  );
  content.append(head);

  content.append(create("div", { className: "subj", textContent: item.subject || "(无主题)" }));

  const snippet = create("div", { className: "snippet" });
  // 压缩空白后截断；`textContent` 赋值保证不会把预览当 HTML 解析。
  snippet.append(create("span", {
    className: "snippet-text",
    textContent: item.preview.replace(/\s+/g, " ").trim().slice(0, 120) || "暂无正文预览",
  }));
  const alias = recipientAlias(item.recipient);
  if (alias) {
    const to = create("span", { className: "mail-alias", title: `收件地址：${item.recipient}` });
    to.append(create("span", { className: "alias-prefix", textContent: "to:" }), document.createTextNode(alias));
    snippet.append(to);
  }
  if (item.attachment_count > 0) {
    const tag = create("span", { className: "attachment-tag", title: `${item.attachment_count} 个附件` });
    tag.append(icon("paperclip"), document.createTextNode(String(item.attachment_count)));
    snippet.append(tag);
  }
  content.append(snippet);
  row.append(content);
  row.addEventListener("click", () => onOpen(item.id));
  return row;
}
