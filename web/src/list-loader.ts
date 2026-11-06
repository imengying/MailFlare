import type { EmailList } from "./api-types.ts";
import type { ApiClient } from "./api-client.ts";
import type { Filter } from "./state.ts";

/** 所有列表请求共享序号；较旧的响应永远不能覆盖较新的筛选。 */
export class ListLoader {
  private generation = 0;
  invalidate(): void { this.generation += 1; }
  async load(api: ApiClient, filter: Filter, search: string, count?: number, cursor?: string): Promise<EmailList | null> {
    const generation = ++this.generation;
    const items: EmailList["items"][number][] = [];
    let next = cursor;
    for (;;) {
      const params = new URLSearchParams({ filter, q: search });
      if (count !== undefined) params.set("limit", String(Math.min(100, Math.max(1, count - items.length))));
      if (next) params.set("cursor", next);
      let data: EmailList;
      try { data = await api.request<EmailList>(`/api/emails?${params}`); }
      catch (error) { if (generation !== this.generation) return null; throw error; }
      if (generation !== this.generation) return null;
      items.push(...data.items);
      if (count === undefined || !data.next_cursor || items.length >= count || data.items.length === 0 || data.next_cursor === next) {
        return { items, unread: data.unread, next_cursor: data.next_cursor };
      }
      next = data.next_cursor;
    }
  }
}
