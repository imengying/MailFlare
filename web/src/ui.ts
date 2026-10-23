/**
 * 通用 UI 基元：Toast 与空状态。
 *
 * 单独成模块是因为列表、详情、设置三处都要用，且它们不依赖任何业务状态。
 */

import { create } from "./dom.ts";

let toastTimer: ReturnType<typeof setTimeout> | null = null;

/**
 * 底部浮动提示。
 *
 * 用 `role="status"` + `aria-live="polite"`（写在 HTML 上）让读屏软件能读到，
 * 而不只是视觉提示。
 */
export function toast(message: string, durationMs = 2400): void {
  const node = document.querySelector<HTMLElement>("#toast");
  if (!node) return;

  node.textContent = message;
  node.classList.add("show");

  if (toastTimer !== null) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    node.classList.remove("show");
    toastTimer = null;
  }, durationMs);
}

/** 构造一个居中的空状态占位块。 */
export function emptyState(text: string): HTMLElement {
  const box = create("div", { className: "empty" });
  box.append(create("div", { className: "big", textContent: "✉" }));
  box.append(document.createTextNode(text));
  return box;
}

/** 构造一组骨架屏行（加载列表时用）。 */
export function skeletonRows(count: number): HTMLElement[] {
  return Array.from({ length: count }, () => {
    const row = create("div", { className: "mail-item" });
    row.append(
      create("div", {
        className: "skeleton",
        style: "height:12px;width:45%;margin-bottom:8px",
      }),
      create("div", { className: "skeleton", style: "height:12px;width:80%" }),
    );
    return row;
  });
}
