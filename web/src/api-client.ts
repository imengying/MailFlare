/**
 * 与后端通信的薄封装。
 *
 * 职责：
 * 1. 统一注入 `Authorization` 头；
 * 2. 解包 `{ok, data}` / `{ok, error}` 两种响应；
 * 3. 401 时自动清 token 并回到登录页（通过注入的 `onUnauthorized` 回调，
 *    避免这里反向依赖 UI 模块造成循环引用）。
 */

import type { ErrEnvelope, OkEnvelope } from "./api-types.ts";

/** 后端返回非 2xx 时抛出的错误，带 HTTP 状态码便于上层区分处理。 */
export class ApiError extends Error {
  readonly status: number;

  constructor(message: string, status: number) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

export interface ApiOptions {
  readonly method?: "GET" | "POST" | "PUT" | "DELETE";
  /** 会被 JSON 序列化后作为请求体。 */
  readonly body?: unknown;
}

export interface ApiClient {
  /** 发起请求并返回解包后的 `data`。 */
  request<T>(path: string, options?: ApiOptions): Promise<T>;
  /** 发起请求并返回原始 Response（用于下载等不需要 JSON 解包的场景）。 */
  raw(path: string, options?: ApiOptions): Promise<Response>;
  getToken(): string;
  setToken(token: string): void;
}

const TOKEN_KEY = "mf_token";

/**
 * 创建 API 客户端。
 *
 * @param onUnauthorized token 失效（401）时的回调，通常是「回到登录页」。
 */
export function createApi(onUnauthorized: () => void): ApiClient {
  let token = localStorage.getItem(TOKEN_KEY) ?? "";

  const authHeaders = (extra?: Record<string, string>): Record<string, string> => {
    const headers: Record<string, string> = { ...extra };
    if (token) headers["Authorization"] = `Bearer ${token}`;
    return headers;
  };

  const doFetch = async (path: string, options: ApiOptions = {}): Promise<Response> => {
    const { method = "GET", body } = options;
    try {
      return await fetch(path, {
        method,
        headers: authHeaders(body === undefined ? undefined : { "Content-Type": "application/json" }),
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
    } catch {
      // fetch 只有在网络层失败时才 reject；HTTP 错误码不会走这里。
      throw new ApiError("网络异常，请检查连接", 0);
    }
  };

  const checkUnauthorized = (resp: Response): void => {
    if (resp.status === 401) {
      token = "";
      localStorage.removeItem(TOKEN_KEY);
      onUnauthorized();
    }
  };

  return {
    getToken: (): string => token,

    setToken(next: string): void {
      token = next;
      if (next) {
        localStorage.setItem(TOKEN_KEY, next);
      } else {
        localStorage.removeItem(TOKEN_KEY);
      }
    },

    async raw(path: string, options?: ApiOptions): Promise<Response> {
      const resp = await doFetch(path, options);
      checkUnauthorized(resp);
      if (!resp.ok) {
        throw new ApiError(`请求失败（HTTP ${resp.status}）`, resp.status);
      }
      return resp;
    },

    async request<T>(path: string, options?: ApiOptions): Promise<T> {
      const resp = await doFetch(path, options);
      checkUnauthorized(resp);

      // 空响应体（如 204）直接当成功处理，避免 JSON 解析抛错。
      if (resp.status === 204) {
        return undefined as T;
      }

      let parsed: unknown = null;
      try {
        parsed = await resp.json();
      } catch {
        // 非 JSON 响应：下面按状态码兜底。
      }

      if (!resp.ok) {
        const message =
          isErrEnvelope(parsed) ? parsed.error : `请求失败（HTTP ${resp.status}）`;
        throw new ApiError(message, resp.status);
      }

      if (isOkEnvelope<T>(parsed)) {
        return parsed.data;
      }

      // 后端理应总是包一层 `{ok, data}`；这里容忍裸对象，避免契约微调就把 UI 打挂。
      return parsed as T;
    },
  };
}

function isOkEnvelope<T>(value: unknown): value is OkEnvelope<T> {
  return (
    typeof value === "object" &&
    value !== null &&
    "ok" in value &&
    (value as { ok: unknown }).ok === true
  );
}

function isErrEnvelope(value: unknown): value is ErrEnvelope {
  return (
    typeof value === "object" &&
    value !== null &&
    "ok" in value &&
    (value as { ok: unknown }).ok === false &&
    typeof (value as { error?: unknown }).error === "string"
  );
}
