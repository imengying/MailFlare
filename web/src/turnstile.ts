/**
 * Cloudflare Turnstile 挂件封装。
 *
 * 只在后端返回 `turnstile_required: true` 时才加载脚本；未配置 secret 的
 * 本地开发环境完全不请求外部脚本。
 */

/** Turnstile 官方全局对象（`api.js?render=explicit` 注入）。 */
interface TurnstileApi {
  render(
    container: string | HTMLElement,
    options: {
      sitekey: string;
      theme?: "light" | "dark" | "auto";
      callback?: (token: string) => void;
      "expired-callback"?: () => void;
      "error-callback"?: () => void;
    },
  ): string;
  reset(widgetId?: string): void;
}

declare global {
  interface Window {
    turnstile?: TurnstileApi;
  }
}

const SCRIPT_URL = "https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit";

/** 挂件控制器：持有当前 token，并支持重置。 */
export interface TurnstileWidget {
  /** 当前有效 token；为空串表示还没完成验证或已过期。 */
  getToken(): string;
  /** 验证过期/失败/换人后重置，让用户重新点一次。 */
  reset(): void;
  /** 挂件是否已成功初始化。 */
  readonly ready: boolean;
}

/** 创建一个「永不完成」的空挂件，用于 `turnstile_required: false` 的情况。 */
export function noopWidget(): TurnstileWidget {
  return { getToken: () => "", reset: () => {}, ready: false };
}

/**
 * 加载 Turnstile 并渲染挂件。
 *
 * @param siteKey 后端 `/api/auth/config` 下发的公开 site key
 * @param container 挂载容器（CSS 选择器或元素）
 * @param theme 配色，跟随系统
 * @param onLoadError 脚本加载失败时的回调（用于提示用户）
 */
export async function mountTurnstile(
  siteKey: string,
  container: string | HTMLElement,
  theme: "light" | "dark",
  onLoadError: () => void,
): Promise<TurnstileWidget> {
  await loadScript();
  const api = window.turnstile;
  if (!api) {
    onLoadError();
    return noopWidget();
  }

  let token = "";
  const widgetId = api.render(container, {
    sitekey: siteKey,
    theme,
    callback: (value: string) => {
      token = value;
    },
    "expired-callback": () => {
      token = "";
    },
    "error-callback": () => {
      token = "";
    },
  });

  return {
    getToken: () => token,
    reset(): void {
      token = "";
      try {
        api.reset(widgetId);
      } catch {
        // 挂件已被移除时 reset 会抛，忽略即可。
      }
    },
    ready: true,
  };
}

/** 幂等地插入 `<script>`；多次调用共享同一个加载 Promise。 */
let scriptPromise: Promise<void> | null = null;

function loadScript(): Promise<void> {
  if (window.turnstile) return Promise.resolve();
  if (scriptPromise) return scriptPromise;

  scriptPromise = new Promise<void>((resolve, reject) => {
    const script = document.createElement("script");
    script.src = SCRIPT_URL;
    script.async = true;
    script.defer = true;
    script.addEventListener("load", () => resolve(), { once: true });
    script.addEventListener(
      "error",
      () => {
        scriptPromise = null; // 允许用户重试
        reject(new Error("Turnstile 脚本加载失败"));
      },
      { once: true },
    );
    document.head.appendChild(script);
  });

  return scriptPromise;
}
