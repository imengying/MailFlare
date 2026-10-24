/** Toast、空状态与骨架屏。 */

import { create, el } from "./dom.ts";
import { icon, type IconName } from "./icons.ts";

let toastTimer: ReturnType<typeof setTimeout> | null = null;

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

/** 空状态支持独立标题与补充说明，错误信息不会被挤进一个小图标里。 */
export function emptyState(
  title: string,
  description = "",
  iconName: IconName = "mail",
): HTMLElement {
  const box = create("div", { className: "empty" });
  const graphic = create("div", { className: "empty-icon" });
  graphic.append(icon(iconName));
  box.append(graphic, create("h2", { textContent: title }));
  if (description) box.append(create("p", { textContent: description }));
  return box;
}

export function skeletonRows(count: number): HTMLElement[] {
  return Array.from({ length: count }, () => {
    const row = create("div", { className: "mail-item skeleton-row" });
    row.append(
      create("div", { className: "skeleton", style: "height:12px;width:48%;margin-bottom:12px" }),
      create("div", { className: "skeleton", style: "height:13px;width:82%;margin-bottom:9px" }),
      create("div", { className: "skeleton", style: "height:11px;width:68%" }),
    );
    return row;
  });
}

/** 主界面初次进入或删除当前邮件后的统一占位。 */
export function renderDetailEmpty(): void {
  el<HTMLElement>("#detail-pane").replaceChildren(
    emptyState("选择一封邮件", "在左侧选择邮件，即可阅读正文与管理附件"),
  );
}

/** 危险操作确认框；原生 dialog 提供焦点约束与 Esc 支持。 */
export function confirmDialog(
  title: string,
  description: string,
  confirmText: string,
): Promise<boolean> {
  const dialog = create("dialog", { className: "confirm-dialog" });
  dialog.setAttribute("aria-labelledby", "confirm-title");
  dialog.setAttribute("aria-describedby", "confirm-description");
  const content = create("div", { className: "confirm-content" });
  const symbol = create("div", { className: "confirm-icon" });
  symbol.append(icon("trash"));
  content.append(
    symbol,
    create("h2", { id: "confirm-title", textContent: title }),
    create("p", { id: "confirm-description", textContent: description }),
  );
  const actions = create("div", { className: "confirm-actions" });
  const cancel = create("button", { className: "btn", type: "button", textContent: "取消" });
  const confirm = create("button", { className: "btn danger-solid", type: "button", textContent: confirmText });
  actions.append(cancel, confirm);
  content.append(actions);
  dialog.append(content);
  document.body.append(dialog);

  return new Promise((resolve) => {
    let settled = false;
    const finish = (value: boolean): void => {
      if (settled) return;
      settled = true;
      dialog.close();
      dialog.remove();
      resolve(value);
    };
    cancel.addEventListener("click", () => finish(false));
    confirm.addEventListener("click", () => finish(true));
    dialog.addEventListener("cancel", (event) => {
      event.preventDefault();
      finish(false);
    });
    dialog.addEventListener("click", (event) => {
      if (event.target !== dialog) return;
      const rect = dialog.getBoundingClientRect();
      if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) {
        finish(false);
      }
    });
    dialog.showModal();
    cancel.focus();
  });
}
