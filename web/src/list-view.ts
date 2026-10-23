/**
 * 邮件列表渲染。
 *
 * 纯渲染函数：只读 `AppState`，不改它。所有交互通过回调注入，
 * 这样渲染逻辑可以独立测试，也不会和网络层纠缠。
 */

import type { EmailSummary } from "./api-types.ts";
import { create, el, formatTime } from "./dom.ts";
import type { AppState } from "./state.ts";
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

/** 更新侧边栏未读角标。独立出来，因为「标为已读」时不需要重绘整个列表。 */
export function renderUnreadBadge(state: AppState): void {
  const badge = el<HTMLElement>("#count-unread");
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

  if (state.loadingList) {
    pane.replaceChildren(...skeletonRows(6));
    return;
  }

  const items = visibleItems(state);
  if (items.length === 0) {
    pane.replaceChildren(emptyState(state.search ? "没有匹配的邮件" : "这里还没有邮件"));
    return;
  }

  const frag = document.createDocumentFragment();
  for (const item of items) {
    frag.append(buildRow(item, state.selectedId === item.id, handlers.onOpen));
  }

  // 「加载更多」而不是无限滚动：行为可预测，也便于整页可见性判断。
  if (state.nextCursor !== null) {
    const more = create("button", {
      className: "btn ghost",
      style: "width:100%;border-radius:0;padding:14px",
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
  });

  const head = create("div", { className: "row1" });
  head.append(
    create("span", { className: "from", textContent: item.sender || "(未知发件人)" }),
    create("span", { className: "when", textContent: formatTime(item.created_at) }),
  );
  row.append(head);

  row.append(create("div", { className: "subj", textContent: item.subject || "(无主题)" }));

  const snippet = create("div", { className: "snippet" });
  // 压缩空白后截断；`textContent` 赋值保证不会把预览当 HTML 解析。
  snippet.textContent = item.preview.replace(/\s+/g, " ").slice(0, 120);
  if (item.attachment_count > 0) {
    snippet.append(create("span", { className: "clip", textContent: `  📎${item.attachment_count}` }));
  }
  row.append(snippet);

  row.addEventListener("click", () => onOpen(item.id));
  return row;
}
