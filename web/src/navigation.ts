/** 同页邮件导航：URL 保存筛选、搜索和选中项；历史索引用于取消离开设置。 */
import type { Filter } from "./state.ts";

export interface MailRoute {
  filter: Filter;
  id: string | null;
  search: string;
}
interface Entry {
  session: string;
  index: number;
  hash: string;
  scroll: number;
}
function readRoute(hash: string): MailRoute {
  const [path = "", query = ""] = hash.replace(/^#/, "").split("?", 2);
  const [filter, id] = path.split("/", 2);
  try {
    return {
      filter: filter === "unread" || filter === "trash" ? filter : "all",
      id: id ? decodeURIComponent(id) : null,
      search: new URLSearchParams(query).get("q") ?? "",
    };
  } catch { return { filter: "all", id: null, search: "" }; }
}
function hashFor(route: MailRoute): string {
  const query = new URLSearchParams();
  if (route.search) query.set("q", route.search);
  return `#${route.filter}${route.id ? `/${encodeURIComponent(route.id)}` : ""}${query.size ? `?${query}` : ""}`;
}
function historyEntry(): Entry | null {
  const value = (history.state as { mailflare?: Partial<Entry> } | null)?.mailflare;
  return value && typeof value.session === "string" && Number.isInteger(value.index) && Number(value.index) >= 0 && typeof value.hash === "string" && typeof value.scroll === "number"
    ? value as Entry : null;
}

export class MailNavigation {
  private current: Entry;
  private pending: Entry | null = null;
  private deciding = false;
  private restoring: number | null = null;

  constructor(
    private readonly canLeave: () => Promise<boolean>,
    private readonly apply: (route: MailRoute) => Promise<void>,
    private readonly onError: (error: unknown) => void,
  ) {
    const route = readRoute(location.hash);
    const saved = historyEntry();
    this.current = saved && saved.hash === hashFor(route) ? saved
      : { session: crypto.randomUUID(), index: 0, hash: hashFor({ ...route, id: null }), scroll: 0 };
    if (!saved || saved.hash !== hashFor(route)) {
      history.replaceState({ mailflare: this.current }, "", this.current.hash);
      // 直接打开详情链接也先建立列表入口，手机返回不会直接离开站点。
      if (route.id) this.write(route, false);
    }
    const changed = (): void => { void this.onLocation().catch(onError); };
    window.addEventListener("popstate", changed);
    window.addEventListener("hashchange", changed);
  }

  get route(): MailRoute { return readRoute(this.current.hash); }
  get scroll(): number { return this.current.scroll; }
  matches(route: MailRoute): boolean { return this.current.hash === hashFor(route); }

  select(route: MailRoute): void {
    if (!this.route.id) {
      this.current.scroll = document.querySelector("#list-pane")?.scrollTop ?? 0;
      history.replaceState({ mailflare: this.current }, "", this.current.hash);
    }
    this.write(route, this.route.id !== null);
  }

  replace(route: MailRoute): void { this.write(route, true); }

  back(): void {
    if (!this.route.id || this.deciding || this.restoring !== null) return;
    if (this.current.index > 0) history.back();
    else {
      this.replace({ ...this.route, id: null });
      void this.apply(this.route).catch(this.onError);
    }
  }

  private write(route: MailRoute, replace: boolean): void {
    const hash = hashFor(route);
    if (hash === this.current.hash) return;
    this.current = { ...this.current, index: this.current.index + (replace ? 0 : 1), hash };
    history[replace ? "replaceState" : "pushState"]({ mailflare: this.current }, "", hash);
  }

  private async onLocation(): Promise<void> {
    let target = historyEntry();
    const hash = hashFor(readRoute(location.hash));
    // 手动修改 hash 会新建没有标记的历史条目；补上索引，仍然可以正常返回。
    if (!target || target.session !== this.current.session || target.hash !== hash) {
      target = { ...this.current, index: this.current.index + 1, hash, scroll: 0 };
      history.replaceState({ mailflare: target }, "", hash);
    }
    if (this.restoring !== null) {
      if (this.restoring === target.index) this.restoring = null;
      return;
    }
    if (this.deciding) { this.pending = target; return; }
    if (target.index === this.current.index && target.hash === this.current.hash) return;
    this.pending = target;
    this.deciding = true;
    let allowed = false;
    try { allowed = await this.canLeave(); }
    finally { this.deciding = false; }
    const destination = this.pending!;
    this.pending = null;
    if (!allowed) {
      if (destination.index === this.current.index) return;
      // 回到原有历史条目，不 push 新条目，因此取消后前进/后退仍然对称。
      this.restoring = this.current.index;
      history.go(this.current.index - destination.index);
      return;
    }
    this.current = destination;
    await this.apply(this.route);
  }
}
