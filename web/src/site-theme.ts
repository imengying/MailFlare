/**
 * 站点外观：站点名、Logo、登录页背景。
 *
 * 这些值来自 `/api/auth/config`（登录前就可用）与 `/api/settings`。
 * 后端已经校验过地址合法性，但前端仍然只走 `textContent` 与受控的
 * `style.background`，不拼接 HTML。
 */

import { icon } from "./icons.ts";

/** 与后端 `AuthConfigResponse` 的站点字段对应。 */
export interface SiteAppearance {
  readonly site_name: string;
  readonly site_logo: string;
  readonly site_background: string;
}

function defaultLogo(container: HTMLElement): void {
  container.classList.remove("custom");
  container.replaceChildren(icon("mail"));
}

/**
 * 把 Logo 容器渲染成默认 SVG 图标或自定义图片；地址合法性由后端校验。
 */
function applyLogo(container: HTMLElement, source: string): void {
  const value = source.trim();
  if (!value) {
    defaultLogo(container);
    return;
  }

  container.classList.add("custom");
  container.replaceChildren();
  const img = document.createElement("img");
  img.src = value;
  img.alt = "";
  // 图片挂了就退回默认图标，避免出现破图图标。
  img.addEventListener(
    "error",
    () => {
      defaultLogo(container);
    },
    { once: true },
  );
  container.append(img);
}

/** 应用站点名与 Logo 到登录页和侧栏。 */
export function applySiteIdentity(site: SiteAppearance): void {
  const name = site.site_name.trim() || "MailFlare";

  for (const [nameEl, logoEl] of [
    ["#login-name", "#login-logo"],
    ["#sidebar-name", "#sidebar-logo"],
  ] as const) {
    const nameNode = document.querySelector<HTMLElement>(nameEl);
    if (nameNode) nameNode.textContent = name;
    const logoNode = document.querySelector<HTMLElement>(logoEl);
    if (logoNode) applyLogo(logoNode, site.site_logo);
  }

  // 顺带更新页面标题，浏览器标签页也能看到站点名。
  document.title = name;
}

/** 应用登录页背景。空值表示沿用默认渐变。 */
export function applyLoginBackground(background: string): void {
  const view = document.querySelector<HTMLElement>("#login-view");
  if (!view) return;

  const value = background.trim();
  // 后端已把图片地址包成 `url("...")`，颜色值原样透传。
  // 这里只写入 background-image / background-color，不碰其它属性。
  if (value.startsWith("url(")) {
    view.style.backgroundImage = value;
    view.style.backgroundColor = "var(--bg)";
    view.style.backgroundSize = "cover";
    view.style.backgroundPosition = "center";
    view.style.backgroundRepeat = "no-repeat";
  } else if (value) {
    view.style.backgroundImage = "";
    view.style.backgroundColor = value;
  } else {
    // 清空内联样式，回到 CSS 里定义的默认渐变。
    view.style.removeProperty("background-image");
    view.style.removeProperty("background-color");
    view.style.removeProperty("background-size");
    view.style.removeProperty("background-position");
    view.style.removeProperty("background-repeat");
  }
}
