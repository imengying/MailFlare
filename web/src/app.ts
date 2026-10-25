/**
 * 应用主入口。
 *
 * 单向数据流：DOM 事件 → 改 state → render。无框架，
 * 每个 render 函数都用当前 state 重建 DOM。
 */

import type { AuthConfig, EmailDetail, EmailList, LoginResult, SessionResult } from "./api-types.ts";
import { ApiError, createApi } from "./api-client.ts";
import { renderDetail, renderDetailBody, renderDetailError, renderDetailLoading } from "./detail-view.ts";
import { mountIcons } from "./icons.ts";
import { el, isMobile, prefersDark } from "./dom.ts";
import { renderList } from "./list-view.ts";
import { createSettingsController } from "./settings-view.ts";
import { applyLoginBackground, applySiteIdentity } from "./site-theme.ts";
import { createState } from "./state.ts";
import { mountTurnstile, noopWidget, type TurnstileWidget } from "./turnstile.ts";
import { confirmDialog, renderDetailEmpty, toast } from "./ui.ts";

const state = createState();
const api = createApi(() => showLogin());

let turnstile: TurnstileWidget = noopWidget();
let pollTimer: ReturnType<typeof setInterval> | null = null;
let searchTimer: ReturnType<typeof setTimeout> | null = null;
let detailRequest = 0;
let settingsReturn: { filter: "all" | "unread"; mobileShowing: boolean } = {
  filter: "all",
  mobileShowing: false,
};

// ===========================================================================
// 视图切换
// ===========================================================================

function setSidebarOpen(open: boolean): void {
  const sidebar = el<HTMLElement>("#sidebar");
  const wasOpen = sidebar.classList.contains("open");
  const visible = open && isMobile();
  const main = el<HTMLElement>("main");
  // 即使焦点仍在主区域，也先释放，再把主区域设为 inert。
  if (visible && main.contains(document.activeElement)) {
    (document.activeElement as HTMLElement).blur();
  }
  sidebar.classList.toggle("open", visible);
  main.inert = visible;
  el<HTMLButtonElement>("#toggle-sidebar").setAttribute("aria-label", visible ? "关闭导航" : "打开导航");
  // 侧栏在手机上只是移出屏幕，inert 防止 Tab 聚焦到不可见按钮。
  sidebar.inert = isMobile() && !visible;
  el<HTMLButtonElement>("#sidebar-backdrop").hidden = !visible;
  el<HTMLButtonElement>("#toggle-sidebar").setAttribute("aria-expanded", String(visible));
  if (visible) {
    sidebar.querySelector<HTMLButtonElement>(".nav-item.active")?.focus({ preventScroll: true });
  } else if (wasOpen && isMobile()) {
    el<HTMLButtonElement>("#toggle-sidebar").focus({ preventScroll: true });
  }
}

function closeDetail(): void {
  el<HTMLElement>("#app-view").classList.remove("showing");
  if (isMobile()) el<HTMLElement>("#detail-pane").scrollTop = 0;
}

function syncMailNavigation(): void {
  document.querySelectorAll<HTMLElement>(".nav-item[data-filter]").forEach((node) => {
    const active = !settings.isOpen() && node.dataset["filter"] === state.filter;
    node.classList.toggle("active", active);
    if (active) node.setAttribute("aria-current", "page");
    else node.removeAttribute("aria-current");
  });
}

function setSettingsView(open: boolean): void {
  const app = el<HTMLElement>("#app-view");
  el<HTMLElement>("#settings-view").hidden = !open;
  el<HTMLElement>("#detail-pane").hidden = open;
  app.classList.toggle("settings-open", open);
  el<HTMLElement>("#open-settings").classList.toggle("active", open);
  if (open) {
    settingsReturn = {
      filter: state.filter,
      mobileShowing: app.classList.contains("showing"),
    };
    app.classList.add("showing");
    el<HTMLElement>("#open-settings").setAttribute("aria-current", "page");
  } else {
    state.filter = settingsReturn.filter;
    el<HTMLElement>("#list-title").textContent = state.filter === "unread" ? "未读邮件" : "全部邮件";
    app.classList.toggle("showing", settingsReturn.mobileShowing);
    el<HTMLElement>("#open-settings").removeAttribute("aria-current");
  }
  syncMailNavigation();
}

function setLoginBusy(busy: boolean): void {
  const button = el<HTMLButtonElement>("#login-btn");
  button.disabled = busy;
  button.setAttribute("aria-busy", String(busy));
  const label = button.querySelector("span");
  if (label) label.textContent = busy ? "登录中…" : "登录";
}

function showLogin(): void {
  stopPolling();
  detailRequest += 1;
  setSidebarOpen(false);
  settings.reset();
  el<HTMLElement>("#app-view").hidden = true;
  el<HTMLElement>("#app-view").classList.remove("showing");
  el<HTMLElement>("#login-view").hidden = false;
  state.detail = null;
  state.selectedId = null;
  state.items = [];
}

async function enterApp(): Promise<void> {
  el<HTMLElement>("#login-view").hidden = true;
  el<HTMLElement>("#app-view").hidden = false;
  setSidebarOpen(false);
  try {
    await loadList({ reset: true });
  } catch (error) {
    toast(errorMessage(error));
  }
  startPolling();
  void refreshAppearance();
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
  // 搜索交给后端：只过滤前端已加载的那几条会漏掉更早的邮件。
  if (state.search) params.set("q", state.search);
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
    if (cursor !== null && !state.loadingMore && !state.loadingList) {
      void loadList({ cursor }).catch((error: unknown) => toast(errorMessage(error)));
    }
  },
};

// ===========================================================================
// 详情
// ===========================================================================

async function openEmail(id: string): Promise<void> {
  if (settings.isOpen() && !(await settings.close())) return;
  const request = ++detailRequest;
  state.selectedId = id;
  state.allowRemoteImages = false;
  el<HTMLElement>("#app-view").classList.add("showing");
  renderList(state, listHandlers);
  renderDetailLoading();

  try {
    const detail = await api.request<EmailDetail>(`/api/emails/${encodeURIComponent(id)}`);
    if (request !== detailRequest || state.selectedId !== id) return;
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
    if (request !== detailRequest) return;
    toast(errorMessage(error));
    renderDetailError(errorMessage(error));
  }
}

const detailHandlers = {
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
  onCopyOtp(code: string): void {
    void copyText(code);
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
      closeDetail();
    }
  } catch (error) {
    toast(errorMessage(error));
  }
}

async function deleteEmail(id: string): Promise<void> {
  const detail = state.detail;
  if (!detail || detail.id !== id) return;

  const confirmed = await confirmDialog(
    "删除这封邮件？",
    `「${detail.subject || "无主题"}」的正文与已保存附件将一并删除，且无法恢复。`,
    "删除邮件",
  );
  if (!confirmed || state.detail?.id !== id) return;

  const index = state.items.findIndex((item) => item.id === id);
  try {
    await api.request(`/api/emails/${encodeURIComponent(id)}`, { method: "DELETE" });
    detailRequest += 1;
    state.items = state.items.filter((it) => it.id !== id);
    state.detail = null;
    state.selectedId = null;
    renderList(state, listHandlers);
    const next = state.items[Math.min(Math.max(index, 0), state.items.length - 1)];
    if (!isMobile() && next) {
      void openEmail(next.id);
    } else {
      closeDetail();
      renderDetailEmpty();
    }
    toast("已删除");
  } catch (error) {
    toast(errorMessage(error));
  }
}

async function markAllRead(): Promise<void> {
  if (settings.isOpen() && !(await settings.close())) return;
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

  setLoginBusy(true);

  try {
    const result = await api.request<LoginResult>("/api/auth/login", {
      method: "POST",
      body: {
        username,
        password,
        turnstile_token: turnstile.getToken(),
      },
    });
    api.setToken(result.token);
    state.token = result.token;
    // 只清密码：用户名留在框里，下次登录少打一遍。
    el<HTMLInputElement>("#password").value = "";
    await enterApp();
  } catch (error) {
    errBox.textContent = errorMessage(error);
    // token 是一次性的，失败后必须重新验证，所以把挂件放回来再重置
    // （先显示再 reset：display:none 时挂件算不出尺寸）。
    if (turnstile.ready) el<HTMLElement>("#turnstile-field").hidden = false;
    turnstile.reset();
  } finally {
    setLoginBusy(false);
  }
}

/**
 * 读取登录前的公开配置：应用站点外观 + 按需初始化 Turnstile。
 *
 * 站点外观必须在登录前生效，所以和 Turnstile 共用这个不需要鉴权的接口。
 */
async function initLoginPage(): Promise<void> {
  try {
    const config = await api.request<AuthConfig>("/api/auth/config");
    applySiteIdentity(config);
    applyLoginBackground(config.site_background);

    if (!config.turnstile_required || !config.turnstile_site_key) return;

    el<HTMLElement>("#turnstile-field").hidden = false;
    turnstile = await mountTurnstile(
      config.turnstile_site_key,
      "#turnstile-container",
      prefersDark() ? "dark" : "light",
      () => {
        toast("人机验证加载失败，请检查网络");
      },
      // 验证通过就收起挂件：token 已经在手上，没必要继续占地方。
      () => {
        el<HTMLElement>("#turnstile-field").hidden = true;
      },
    );
  } catch {
    // 拿不到配置就用默认外观，隐藏验证框；登录接口会给出明确错误。
  }
}

/** 重新拉取公开配置并应用外观（保存后或登录后调用）。 */
async function refreshAppearance(): Promise<void> {
  try {
    const config = await api.request<AuthConfig>("/api/auth/config");
    applySiteIdentity(config);
    applyLoginBackground(config.site_background);
  } catch {
    // 外观刷新失败不影响主流程。
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

const settings = createSettingsController(
  api,
  (loaded) => {
    applySiteIdentity(loaded);
  },
  setSettingsView,
);

async function changeFilter(button: HTMLElement): Promise<void> {
  if (settings.isOpen() && !(await settings.close())) return;
  const filter = button.dataset["filter"];
  state.filter = filter === "unread" ? "unread" : "all";
  el<HTMLElement>("#list-title").textContent = state.filter === "unread" ? "未读邮件" : "全部邮件";
  syncMailNavigation();
  setSidebarOpen(false);
  detailRequest += 1;
  state.selectedId = null;
  state.detail = null;
  renderDetailEmpty();
  closeDetail();
  await loadList({ reset: true });
}

function bindEvents(): void {
  el<HTMLFormElement>("#login-form").addEventListener("submit", (event) => {
    void handleLogin(event);
  });

  el<HTMLFormElement>("#settings-form").addEventListener("submit", (event) => {
    event.preventDefault();
    void settings.save();
  });

  // 保存成功后重新应用外观，停留在右侧设置工作区。
  el<HTMLElement>("#settings-view").addEventListener("settings-saved", () => {
    toast("设置已保存");
    void refreshAppearance();
  });

  // 手机端设置页会隐藏邮件工具栏，保留一个菜单键打开侧栏，
  // 否则无法在设置与其他视图之间切换。
  el<HTMLElement>("#settings-menu").addEventListener("click", () => {
    setSidebarOpen(true);
  });

  el<HTMLElement>("#open-settings").addEventListener("click", () => {
    setSidebarOpen(false);
    if (settings.isOpen()) {
      void settings.close();
      return;
    }
    if (api.getToken()) {
      void settings.open().catch((error: unknown) => toast(errorMessage(error)));
    }
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
      void changeFilter(button).catch((error: unknown) => toast(errorMessage(error)));
    });
  });

  el<HTMLElement>("#mark-all-read").addEventListener("click", () => {
    void markAllRead();
  });

  el<HTMLButtonElement>("#refresh").addEventListener("click", (event) => {
    const button = event.currentTarget as HTMLButtonElement;
    void (async () => {
      if (settings.isOpen() && !(await settings.close())) return;
      button.disabled = true;
      button.classList.add("is-spinning");
      try {
        await loadList({ reset: true });
        toast("已刷新");
        if (state.selectedId !== null) await openEmail(state.selectedId);
      } catch (error) {
        toast(errorMessage(error));
      } finally {
        button.disabled = false;
        button.classList.remove("is-spinning");
      }
    })();
  });

  el<HTMLInputElement>("#search").addEventListener("input", (event) => {
    const value = (event.target as HTMLInputElement).value.trim();
    if (searchTimer !== null) clearTimeout(searchTimer);
    // 防抖：连续输入时不要每敲一个字就重排整个列表。
    searchTimer = setTimeout(() => {
      state.search = value;
      // 关键词变了就得重新取第一页，不能只在本地列表里筛。
      void loadList({ reset: true });
    }, 220);
  });

  el<HTMLElement>("#toggle-sidebar").addEventListener("click", () => {
    setSidebarOpen(!el<HTMLElement>("#sidebar").classList.contains("open"));
  });
  el<HTMLElement>("#sidebar-backdrop").addEventListener("click", () => setSidebarOpen(false));
  window.matchMedia("(max-width: 860px)").addEventListener("change", () => setSidebarOpen(false));

  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    if (settings.isOpen()) {
      void settings.close();
      return;
    }
    if (el<HTMLElement>("#sidebar").classList.contains("open")) {
      setSidebarOpen(false);
      return;
    }
    closeDetail();
  });

  // 从后台切回来时立刻刷新，而不是等下一个 60 秒周期。
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && api.getToken()) {
      void loadList({ reset: true }).catch((error: unknown) => toast(errorMessage(error)));
    }
  });
}

// ===========================================================================
// 启动
// ===========================================================================

async function boot(): Promise<void> {
  mountIcons();
  renderDetailEmpty();
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
  await initLoginPage();
}

void boot();
