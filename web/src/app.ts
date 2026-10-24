/**
 * 应用主入口。
 *
 * 单向数据流：DOM 事件 → 改 state → render。无框架，
 * 每个 render 函数都用当前 state 重建 DOM。
 */

import type { AuthConfig, EmailDetail, EmailList, LoginResult, SessionResult } from "./api-types.ts";
import { ApiError, createApi } from "./api-client.ts";
import { renderDetail, renderDetailBody, renderDetailError, renderDetailLoading } from "./detail-view.ts";
import { el, isMobile, prefersDark } from "./dom.ts";
import { renderList } from "./list-view.ts";
import { createSettingsController } from "./settings-view.ts";
import { createState } from "./state.ts";
import { mountTurnstile, noopWidget, type TurnstileWidget } from "./turnstile.ts";
import { toast } from "./ui.ts";

const state = createState();
const api = createApi(() => showLogin());

let turnstile: TurnstileWidget = noopWidget();
let pollTimer: ReturnType<typeof setInterval> | null = null;
let searchTimer: ReturnType<typeof setTimeout> | null = null;

// ===========================================================================
// 视图切换
// ===========================================================================

function showLogin(): void {
  stopPolling();
  el<HTMLElement>("#app-view").hidden = true;
  el<HTMLElement>("#login-view").hidden = false;
  el<HTMLElement>("#settings-mask").hidden = true;
  state.detail = null;
  state.selectedId = null;
  state.items = [];
}

async function enterApp(): Promise<void> {
  el<HTMLElement>("#login-view").hidden = true;
  el<HTMLElement>("#app-view").hidden = false;
  try {
    await loadList({ reset: true });
  } catch (error) {
    toast(errorMessage(error));
  }
  startPolling();
}

// ===========================================================================
// 列表
// ===========================================================================

async function loadList(options: { reset?: boolean; cursor?: string } = {}): Promise<void> {
  const { reset = false, cursor } = options;

  if (reset) {
    state.loadingList = true;
    renderList(state, listHandlers);
  } else {
    state.loadingMore = true;
  }

  const params = new URLSearchParams();
  if (state.filter === "unread") params.set("filter", "unread");
  if (cursor !== undefined) params.set("cursor", cursor);

  try {
    const data = await api.request<EmailList>(`/api/emails?${params.toString()}`);
    state.items = reset ? data.items : [...state.items, ...data.items];
    state.nextCursor = data.next_cursor;
    state.unread = data.unread;
  } finally {
    state.loadingList = false;
    state.loadingMore = false;
    renderList(state, listHandlers);
  }
}

const listHandlers = {
  onOpen: (id: string): void => {
    void openEmail(id);
  },
  onLoadMore: (): void => {
    // 先取到局部常量：闭包里访问 `state.nextCursor` 时 TS 无法保留收窄结果。
    const cursor = state.nextCursor;
    if (cursor !== null) {
      void loadList({ cursor });
    }
  },
};

// ===========================================================================
// 详情
// ===========================================================================

async function openEmail(id: string): Promise<void> {
  state.selectedId = id;
  state.activeTab = "html";
  state.allowRemoteImages = false;
  el<HTMLElement>("#app-view").classList.add("showing");
  renderList(state, listHandlers);
  renderDetailLoading();

  try {
    const detail = await api.request<EmailDetail>(`/api/emails/${encodeURIComponent(id)}`);
    state.detail = detail;
    renderDetail(state, detailHandlers);

    // 后端已在 GET 详情时标为已读，这里同步本地状态，避免重拉整个列表。
    const item = state.items.find((it) => it.id === id);
    if (item && !item.is_read) {
      item.is_read = true;
      state.unread = Math.max(0, state.unread - 1);
      renderList(state, listHandlers);
    }
  } catch (error) {
    toast(errorMessage(error));
    renderDetailError(errorMessage(error));
  }
}

const detailHandlers = {
  onTabChange(tab: "html" | "text"): void {
    state.activeTab = tab;
    if (state.detail) renderDetail(state, detailHandlers);
  },
  onAllowImages(): void {
    state.allowRemoteImages = true;
    renderDetailBody(state, detailHandlers);
  },
  onMarkUnread(id: string): void {
    void markUnread(id);
  },
  onDelete(id: string): void {
    void deleteEmail(id);
  },
  onCopyMessageId(messageId: string): void {
    void copyText(messageId);
  },
};

async function markUnread(id: string): Promise<void> {
  try {
    await api.request(`/api/emails/${encodeURIComponent(id)}/read`, {
      method: "POST",
      body: { is_read: false },
    });
    const item = state.items.find((it) => it.id === id);
    if (item && item.is_read) {
      item.is_read = false;
      state.unread += 1;
      renderList(state, listHandlers);
      toast("已标为未读");
    }
    if (isMobile()) {
      el<HTMLElement>("#app-view").classList.remove("showing");
    }
  } catch (error) {
    toast(errorMessage(error));
  }
}

async function deleteEmail(id: string): Promise<void> {
  const detail = state.detail;
  if (!detail || detail.id !== id) return;

  const confirmed = confirm(
    `确定删除这封邮件？\n\n主题：${detail.subject}\n\n附件与原始邮件也会一并从 R2 删除。`,
  );
  if (!confirmed) return;

  try {
    await api.request(`/api/emails/${encodeURIComponent(id)}`, { method: "DELETE" });
    state.items = state.items.filter((it) => it.id !== id);
    state.detail = null;
    state.selectedId = null;
    el<HTMLElement>("#app-view").classList.remove("showing");
    document.querySelector("#detail-pane")?.replaceChildren();
    renderList(state, listHandlers);
    toast("已删除");
  } catch (error) {
    toast(errorMessage(error));
  }
}

async function markAllRead(): Promise<void> {
  if (state.unread === 0) {
    toast("没有未读邮件");
    return;
  }
  try {
    await api.request("/api/emails/read-all", { method: "POST" });
    state.items.forEach((item) => {
      item.is_read = true;
    });
    state.unread = 0;
    renderList(state, listHandlers);
    toast("已全部标为已读");
  } catch (error) {
    toast(errorMessage(error));
  }
}

// ===========================================================================
// 登录
// ===========================================================================

async function handleLogin(event: Event): Promise<void> {
  event.preventDefault();

  const button = el<HTMLButtonElement>("#login-btn");
  const errBox = el<HTMLElement>("#login-err");
  errBox.textContent = "";

  const username = el<HTMLInputElement>("#username").value.trim();
  const password = el<HTMLInputElement>("#password").value;
  if (!username) {
    errBox.textContent = "请输入用户名";
    return;
  }
  if (!password) {
    errBox.textContent = "请输入密码";
    return;
  }

  button.disabled = true;
  button.textContent = "登录中…";

  try {
    const result = await api.request<LoginResult>("/api/auth/login", {
      method: "POST",
      body: {
        username,
        password,
        turnstile_token: turnstile.getToken(),
        remember: el<HTMLInputElement>("#remember-me").checked,
      },
    });
    api.setToken(result.token);
    state.token = result.token;
    // 只清密码：用户名留在框里，下次登录少打一遍。
    el<HTMLInputElement>("#password").value = "";
    await enterApp();
  } catch (error) {
    errBox.textContent = errorMessage(error);
    turnstile.reset();
  } finally {
    button.disabled = false;
    button.textContent = "登 录";
  }
}

/** 读取 Turnstile 公开配置并按需初始化挂件。 */
async function initTurnstile(): Promise<void> {
  try {
    const config = await api.request<AuthConfig>("/api/auth/config");
    if (!config.turnstile_required || !config.turnstile_site_key) return;

    el<HTMLElement>("#turnstile-field").hidden = false;
    turnstile = await mountTurnstile(
      config.turnstile_site_key,
      "#turnstile-container",
      prefersDark() ? "dark" : "light",
      () => {
        toast("人机验证加载失败，请检查网络");
      },
    );
  } catch {
    // 拿不到配置就保持隐藏；登录接口会返回明确错误。
  }
}

// ===========================================================================
// 轮询
// ===========================================================================

/**
 * 页面可见时每 60 秒静默刷新。
 *
 * 静默 = 不替换整个列表，只更新数据并重绘，保留选中项与滚动位置。
 * `document.hidden` 时跳过，避免后台标签页无谓消耗 D1 配额。
 */
function startPolling(): void {
  stopPolling();
  pollTimer = setInterval(() => {
    void pollOnce();
  }, 60_000);
}

function stopPolling(): void {
  if (pollTimer !== null) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

async function pollOnce(): Promise<void> {
  if (document.hidden || !api.getToken() || state.loadingList) return;

  const params = new URLSearchParams();
  if (state.filter === "unread") params.set("filter", "unread");
  params.set("limit", String(Math.max(state.items.length, 20)));

  try {
    const data = await api.request<EmailList>(`/api/emails?${params.toString()}`);
    const previousTop = state.items[0]?.id;
    const changed =
      data.items.length !== state.items.length ||
      (data.items[0] !== undefined && data.items[0].id !== previousTop) ||
      data.unread !== state.unread;

    if (changed) {
      state.items = data.items;
      state.nextCursor = data.next_cursor;
      state.unread = data.unread;
      renderList(state, listHandlers);
    }
  } catch {
    // 网络抖动忽略，不打断阅读。
  }
}

// ===========================================================================
// 工具
// ===========================================================================

function errorMessage(error: unknown): string {
  if (error instanceof ApiError || error instanceof Error) return error.message;
  return "未知错误";
}

async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    toast("已复制");
  } catch {
    toast("复制失败");
  }
}

// ===========================================================================
// 事件绑定
// ===========================================================================

const settings = createSettingsController(api);

function bindEvents(): void {
  el<HTMLFormElement>("#login-form").addEventListener("submit", (event) => {
    void handleLogin(event);
  });

  el<HTMLFormElement>("#settings-form").addEventListener("submit", (event) => {
    event.preventDefault();
    void settings.save();
  });

  el<HTMLElement>("#settings-cancel").addEventListener("click", () => settings.close());

  el<HTMLElement>("#settings-mask").addEventListener("click", (event) => {
    if (event.target === el<HTMLElement>("#settings-mask")) settings.close();
  });

  // 保存成功由 settings 模块派发事件，避免它直接依赖 toast。
  el<HTMLElement>("#settings-mask").addEventListener("settings-saved", () => {
    toast("设置已保存");
  });

  el<HTMLElement>("#open-settings").addEventListener("click", () => {
    void settings.open();
  });

  el<HTMLElement>("#logout").addEventListener("click", () => {
    void (async () => {
      try {
        await api.request("/api/auth/logout", { method: "POST" });
      } catch {
        // 无状态 JWT，服务端无需处理，失败也继续登出。
      }
      api.setToken("");
      state.token = "";
      showLogin();
    })();
  });

  document.querySelectorAll<HTMLElement>(".nav-item[data-filter]").forEach((button) => {
    button.addEventListener("click", () => {
      document
        .querySelectorAll(".nav-item[data-filter]")
        .forEach((node) => node.classList.remove("active"));
      button.classList.add("active");

      const filter = button.dataset["filter"];
      state.filter = filter === "unread" ? "unread" : "all";
      el<HTMLElement>("#list-title").textContent =
        state.filter === "unread" ? "未读邮件" : "全部邮件";

      el<HTMLElement>("#sidebar").classList.remove("open");
      state.selectedId = null;
      state.detail = null;
      document.querySelector("#detail-pane")?.replaceChildren();
      el<HTMLElement>("#app-view").classList.remove("showing");
      void loadList({ reset: true });
    });
  });

  el<HTMLElement>("#mark-all-read").addEventListener("click", () => {
    void markAllRead();
  });

  el<HTMLElement>("#refresh").addEventListener("click", () => {
    void (async () => {
      try {
        await loadList({ reset: true });
        toast("已刷新");
        if (state.selectedId !== null) await openEmail(state.selectedId);
      } catch (error) {
        toast(errorMessage(error));
      }
    })();
  });

  el<HTMLInputElement>("#search").addEventListener("input", (event) => {
    const value = (event.target as HTMLInputElement).value.trim();
    if (searchTimer !== null) clearTimeout(searchTimer);
    // 防抖：连续输入时不要每敲一个字就重排整个列表。
    searchTimer = setTimeout(() => {
      state.search = value;
      renderList(state, listHandlers);
    }, 120);
  });

  el<HTMLElement>("#toggle-sidebar").addEventListener("click", () => {
    el<HTMLElement>("#sidebar").classList.toggle("open");
  });

  document.addEventListener("keydown", (event) => {
    // `/` 聚焦搜索（输入框里按 `/` 不应触发）。
    if (
      event.key === "/" &&
      document.activeElement !== el<HTMLElement>("#search") &&
      !el<HTMLElement>("#app-view").hidden
    ) {
      event.preventDefault();
      el<HTMLInputElement>("#search").focus();
    }
    if (event.key === "Escape") {
      if (!el<HTMLElement>("#settings-mask").hidden) {
        settings.close();
        return;
      }
      el<HTMLElement>("#app-view").classList.remove("showing");
    }
  });

  // 从后台切回来时立刻刷新，而不是等下一个 60 秒周期。
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && api.getToken()) {
      void loadList({ reset: true });
    }
  });
}

// ===========================================================================
// 启动
// ===========================================================================

async function boot(): Promise<void> {
  bindEvents();

  if (api.getToken()) {
    try {
      await api.request<SessionResult>("/api/auth/session");
      state.token = api.getToken();
      await enterApp();
      return;
    } catch {
      // token 过期，落到登录页。
    }
  }

  showLogin();
  await initTurnstile();
}

void boot();
