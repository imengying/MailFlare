/**
 * 通知设置抽屉。
 *
 * 关键语义（与后端 `SettingsPatch` 的 `Option<T>` 对齐）：
 * - Token 输入框**留空** = 不修改（后端把 `None` 当作「保持不变」）
 * - Token 输入框填入新值 = 覆盖
 * - 后端只回传脱敏值（`*_set` / `*_hint`），所以前端不能把占位符回传
 */

import type { SettingsPatch, SettingsView } from "./api-types.ts";
import type { ApiClient } from "./api-client.ts";
import { el } from "./dom.ts";

export interface SettingsController {
  open(): Promise<void>;
  close(): void;
  /** 保存；成功后自动关闭抽屉。 */
  save(): Promise<void>;
}

export function createSettingsController(api: ApiClient): SettingsController {
  const mask = (): HTMLElement => el<HTMLElement>("#settings-mask");
  const errBox = (): HTMLElement => el<HTMLElement>("#settings-err");

  /** 把服务端配置填进表单。 */
  const fill = (settings: SettingsView): void => {
    el<HTMLInputElement>("#tg-enabled").checked = settings.telegram_enabled;
    el<HTMLInputElement>("#tg-chat").value = settings.telegram_chat_id;
    el<HTMLInputElement>("#bark-enabled").checked = settings.bark_enabled;
    el<HTMLInputElement>("#bark-server").value = settings.bark_server;

    // Token 永不回填：只更新 placeholder 提示「已设置 xxx」。
    const tgToken = el<HTMLInputElement>("#tg-token");
    tgToken.value = "";
    tgToken.placeholder = settings.telegram_bot_token_set
      ? `已设置（${settings.telegram_bot_token_hint}），留空表示不修改`
      : "123456:ABC-DEF...";

    const barkKey = el<HTMLInputElement>("#bark-key");
    barkKey.value = "";
    barkKey.placeholder = settings.bark_device_key_set
      ? `已设置（${settings.bark_device_key_hint}），留空表示不修改`
      : "设备 Key";
  };

  /** 收集表单，只提交非空 token，避免把脱敏占位符写回库。 */
  const collect = (): SettingsPatch => {
    const patch: SettingsPatch = {
      telegram_enabled: el<HTMLInputElement>("#tg-enabled").checked,
      telegram_chat_id: el<HTMLInputElement>("#tg-chat").value.trim(),
      bark_enabled: el<HTMLInputElement>("#bark-enabled").checked,
      bark_server: el<HTMLInputElement>("#bark-server").value.trim(),
    };

    const tgToken = el<HTMLInputElement>("#tg-token").value.trim();
    if (tgToken) patch.telegram_bot_token = tgToken;

    const barkKey = el<HTMLInputElement>("#bark-key").value.trim();
    if (barkKey) patch.bark_device_key = barkKey;

    return patch;
  };

  return {
    async open(): Promise<void> {
      mask().hidden = false;
      errBox().textContent = "";
      try {
        fill(await api.request<SettingsView>("/api/settings"));
      } catch (error) {
        errBox().textContent = error instanceof Error ? error.message : "加载设置失败";
      }
    },

    close(): void {
      mask().hidden = true;
    },

    async save(): Promise<void> {
      const button = el<HTMLButtonElement>("#settings-save");
      errBox().textContent = "";
      button.disabled = true;
      try {
        await api.request<SettingsView>("/api/settings", {
          method: "PUT",
          body: collect(),
        });
        mask().hidden = true;
        // 交由外层 toast，避免这里依赖 ui 模块。
        el<HTMLElement>("#settings-mask").dispatchEvent(
          new CustomEvent("settings-saved", { bubbles: true }),
        );
      } catch (error) {
        errBox().textContent = error instanceof Error ? error.message : "保存失败";
      } finally {
        button.disabled = false;
      }
    },
  };
}
