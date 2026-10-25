/**
 * 应用状态。
 *
 * 刻意用一个可变对象而不是「不可变 + 订阅」：这个界面只有一处渲染入口
 * （用户操作 → 改 state → 调对应 render），引入状态机反而增加理解成本。
 */

import type { EmailDetail, EmailSummary, SettingsView } from "./api-types.ts";

/**
 * 列表项的可变版本。
 *
 * API 契约里的字段是 `readonly`（它们确实不该被外部乱改），但本地为了
 * 「标为已读后同步 UI、避免整页重拉」需要就地改 `is_read`。
 * 用映射类型在状态层解除只读，比放宽整个 DTO 更精确。
 */
export type MutableEmail = { -readonly [K in keyof EmailSummary]: EmailSummary[K] };

/** 列表筛选维度。 */
export type Filter = "all" | "unread";

export interface AppState {
  token: string;
  filter: Filter;
  items: MutableEmail[];
  /** 下一页游标；null 表示已到末页。 */
  nextCursor: string | null;
  /** 未读总数（侧边栏角标）。 */
  unread: number;
  selectedId: string | null;
  detail: EmailDetail | null;
  loadingList: boolean;
  loadingMore: boolean;
  /** 前端本地过滤关键词（不查后端）。 */
  search: string;
  /** 是否允许加载远程图片（默认禁止，防追踪像素）。 */
  allowRemoteImages: boolean;
  settings: SettingsView | null;
}

export function createState(): AppState {
  return {
    token: "",
    filter: "all",
    items: [],
    nextCursor: null,
    unread: 0,
    selectedId: null,
    detail: null,
    loadingList: false,
    loadingMore: false,
    search: "",
    allowRemoteImages: false,
    settings: null,
  };
}
