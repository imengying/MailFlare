/**
 * 右侧设置工作区：邮件列表保持不变，只切换详情列的内容。
 */

import type { SettingsPatch, SettingsView } from "./api-types.ts";
import type { ApiClient } from "./api-client.ts";
import { el, isMobile } from "./dom.ts";
import { confirmDialog } from "./ui.ts";

export interface SettingsController {
  open(): Promise<void>;
  /** 返回 false 表示取消关闭或正在保存。 */
  close(): Promise<boolean>;
  /** 保存后停留在设置工作区。 */
  save(): Promise<void>;
  isOpen(): boolean;
  /** 登出时清除页面状态与未完成的打开请求。 */
  reset(): void;
}

/** 字节 → MB 的换算（表单用 MB 展示，后端用字节存储）。 */
const MB = 1024 * 1024;
const KB = 1024;

/**
 * 把多行文本解析成地址列表。
 *
 * 同时容忍逗号、分号和空白分隔 —— 用户常直接从别处粘贴
 * `a@b.com, c@d.com` 这种一行式的列表。去重由后端统一处理。
 */
function parseList(raw: string): string[] {
  return raw
    .split(/[\n,;]+/)
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

export function createSettingsController(
  api: ApiClient,
  afterOpen?: (settings: SettingsView) => void,
  onViewChange?: (open: boolean) => void,
): SettingsController {
  const view = (): HTMLElement => el<HTMLElement>("#settings-view");
  const form = (): HTMLFormElement => el<HTMLFormElement>("#settings-form");
  const errBox = (): HTMLElement => el<HTMLElement>("#settings-err");
  const text = (sel: string): HTMLInputElement => el<HTMLInputElement>(sel);
  const area = (sel: string): HTMLTextAreaElement => el<HTMLTextAreaElement>(sel);
  const num = (sel: string): HTMLInputElement => el<HTMLInputElement>(sel);
  const check = (sel: string): HTMLInputElement => el<HTMLInputElement>(sel);

  let opened = false;
  let dirty = false;
  let loading = false;
  let saving = false;
  let confirming = false;
  let requestVersion = 0;
  let saveVersion = 0;

  const setDirty = (value: boolean): void => {
    dirty = value;
  };

  const show = (open: boolean): void => {
    opened = open;
    onViewChange?.(open);
    if (open) {
      el<HTMLElement>("#settings-title").focus({ preventScroll: true });
    } else {
      const target = isMobile() ? "#list-title" : "#open-settings";
      el<HTMLElement>(target).focus({ preventScroll: true });
    }
  };

  const close = async (): Promise<boolean> => {
    if (saving || confirming) return false;
    if (!opened) {
      requestVersion += 1;
      loading = false;
      return true;
    }
    if (dirty) {
      confirming = true;
      try {
        const discard = await confirmDialog(
          "放弃未保存的更改？",
          "当前设置尚未保存，返回邮件后将丢失这些修改。",
          "放弃更改",
        );
        if (!discard) return false;
      } finally {
        confirming = false;
      }
    }
    requestVersion += 1;
    setDirty(false);
    show(false);
    return true;
  };

  form().addEventListener("input", () => {
    if (opened) setDirty(true);
  });

  const setControlsDisabled = (disabled: boolean): void => {
    form().querySelectorAll<HTMLInputElement | HTMLTextAreaElement>("input, textarea").forEach((control) => {
      control.disabled = disabled;
    });
    el<HTMLButtonElement>("#settings-save").disabled = disabled;
  };

  /**
   * 密钥输入框的统一处理：永不回填明文，只更新 placeholder。
   *
   * 留空提交时后端会当成 `None`（保持不变），所以用户可以放心
   * 只改其它字段而不动密钥。
   */
  const fillSecret = (
    sel: string,
    isSet: boolean,
    hint: string,
    fallback: string,
  ): void => {
    const input = text(sel);
    input.value = "";
    input.placeholder = isSet ? `已设置（${hint}），留空表示不修改` : fallback;
  };

  /** 把服务端配置填进表单。 */
  const fill = (settings: SettingsView): void => {
    text("#site-name").value = settings.site_name;
    text("#site-logo").value = settings.site_logo;
    text("#site-background").value = settings.site_background;
    text("#public-url").value = settings.public_url;

    text("#ts-site-key").value = settings.turnstile_site_key;
    fillSecret(
      "#ts-secret",
      settings.turnstile_secret_set,
      settings.turnstile_secret_hint,
      "0x4AAA...",
    );

    check("#tg-enabled").checked = settings.telegram_enabled;
    text("#tg-chat").value = settings.telegram_chat_id;
    fillSecret(
      "#tg-token",
      settings.telegram_bot_token_set,
      settings.telegram_bot_token_hint,
      "123456:ABC-DEF...",
    );

    check("#bark-enabled").checked = settings.bark_enabled;
    text("#bark-server").value = settings.bark_server;
    fillSecret(
      "#bark-key",
      settings.bark_device_key_set,
      settings.bark_device_key_hint,
      "设备 Key",
    );

    // R2：账号 ID 与桶名是明文，回填；两个密钥只更新 placeholder。
    text("#r2-account").value = settings.r2_account_id;
    text("#r2-bucket").value = settings.r2_bucket;
    fillSecret(
      "#r2-access-key",
      settings.r2_access_key_id_set,
      settings.r2_access_key_id_hint,
      "R2 API 令牌的 Access Key ID",
    );
    fillSecret(
      "#r2-secret-key",
      settings.r2_secret_access_key_set,
      settings.r2_secret_access_key_hint,
      "R2 API 令牌的 Secret Access Key",
    );

    // 地址列表：一行一条，最直观也最容易从别处整段粘贴。
    area("#blocked-senders").value = settings.blocked_senders.join("\n");
    area("#allowed-recipients").value = settings.allowed_recipients.join("\n");

    num("#max-message").value = String(Math.round(settings.max_message_bytes / MB));
    num("#max-body").value = String(Math.round(settings.max_body_bytes / KB));
    num("#max-attachment").value = String(Math.round(settings.max_attachment_bytes / MB));
    num("#r2-concurrency").value = String(settings.r2_concurrency);

    num("#page-size").value = String(settings.page_size_default);
    num("#page-size-max").value = String(settings.page_size_max);
  };

  /**
   * 读一个数值输入框并换算成目标单位。
   *
   * 输入框可能被用户清空，`Number("")` 是 0 而不是 NaN，所以这里显式
   * 判空后再解析；非法值直接返回 `undefined`，让后端保留原值。
   */
  const readNumber = (sel: string, scale: number): number | undefined => {
    const raw = num(sel).value.trim();
    if (!raw) return undefined;
    const parsed = Number(raw);
    if (!Number.isFinite(parsed) || parsed <= 0) return undefined;
    return Math.round(parsed * scale);
  };

  /** 收集表单；密钥与无效数值留空时保持不变。 */
  const collect = (): SettingsPatch => {
    const patch: SettingsPatch = {
      site_name: text("#site-name").value.trim(),
      site_logo: text("#site-logo").value.trim(),
      site_background: text("#site-background").value.trim(),
      public_url: text("#public-url").value.trim(),
      turnstile_site_key: text("#ts-site-key").value.trim(),
      telegram_enabled: check("#tg-enabled").checked,
      telegram_chat_id: text("#tg-chat").value.trim(),
      bark_enabled: check("#bark-enabled").checked,
      bark_server: text("#bark-server").value.trim(),
      r2_account_id: text("#r2-account").value.trim(),
      r2_bucket: text("#r2-bucket").value.trim(),
      blocked_senders: parseList(area("#blocked-senders").value),
      allowed_recipients: parseList(area("#allowed-recipients").value),
    };

    // 密钥：只在用户真的输入了内容时才提交（留空 = 不修改）。
    const tsSecret = text("#ts-secret").value.trim();
    if (tsSecret) patch.turnstile_secret = tsSecret;

    const tgToken = text("#tg-token").value.trim();
    if (tgToken) patch.telegram_bot_token = tgToken;

    const barkKey = text("#bark-key").value.trim();
    if (barkKey) patch.bark_device_key = barkKey;

    const r2AccessKey = text("#r2-access-key").value.trim();
    if (r2AccessKey) patch.r2_access_key_id = r2AccessKey;

    const r2SecretKey = text("#r2-secret-key").value.trim();
    if (r2SecretKey) patch.r2_secret_access_key = r2SecretKey;

    // 数值项：清空或非法时不提交，后端保留原值。
    const maxMessage = readNumber("#max-message", MB);
    if (maxMessage !== undefined) patch.max_message_bytes = maxMessage;

    const maxBody = readNumber("#max-body", KB);
    if (maxBody !== undefined) patch.max_body_bytes = maxBody;

    const maxAttachment = readNumber("#max-attachment", MB);
    if (maxAttachment !== undefined) patch.max_attachment_bytes = maxAttachment;

    const concurrency = readNumber("#r2-concurrency", 1);
    if (concurrency !== undefined) patch.r2_concurrency = concurrency;

    const pageSize = readNumber("#page-size", 1);
    if (pageSize !== undefined) patch.page_size_default = pageSize;

    const pageSizeMax = readNumber("#page-size-max", 1);
    if (pageSizeMax !== undefined) patch.page_size_max = pageSizeMax;

    return patch;
  };

  return {
    async open(): Promise<void> {
      if (loading || saving || opened) return;
      loading = true;
      const request = ++requestVersion;
      errBox().textContent = "";
      try {
        const loaded = await api.request<SettingsView>("/api/settings");
        if (request !== requestVersion) return;
        fill(loaded);
        setDirty(false);
        el<HTMLElement>(".settings-content").scrollTop = 0;
        show(true);
        afterOpen?.(loaded);
      } catch (error) {
        if (request === requestVersion) {
          errBox().textContent = error instanceof Error ? error.message : "设置加载失败";
          show(true);
        }
        throw error;
      } finally {
        if (request === requestVersion) loading = false;
      }
    },

    close,

    isOpen(): boolean {
      return opened || loading;
    },

    reset(): void {
      requestVersion += 1;
      saveVersion += 1;
      opened = false;
      loading = false;
      saving = false;
      confirming = false;
      setDirty(false);
      setControlsDisabled(false);
      el<HTMLButtonElement>("#settings-save").removeAttribute("aria-busy");
      onViewChange?.(false);
    },

    async save(): Promise<void> {
      if (!opened || saving) return;
      saving = true;
      const request = ++saveVersion;
      const patch = collect();
      const button = el<HTMLButtonElement>("#settings-save");
      errBox().textContent = "";
      setControlsDisabled(true);
      button.setAttribute("aria-busy", "true");
      try {
        const saved = await api.request<SettingsView>("/api/settings", {
          method: "PUT",
          body: patch,
        });
        if (request !== saveVersion) return;
        fill(saved);
        setDirty(false);
        view().dispatchEvent(new CustomEvent("settings-saved", { bubbles: true }));
      } catch (error) {
        if (request !== saveVersion) return;
        errBox().textContent = error instanceof Error ? error.message : "保存失败";
        el<HTMLElement>(".settings-content").scrollTo({ top: 0, behavior: "smooth" });
      } finally {
        if (request === saveVersion) {
          saving = false;
          setControlsDisabled(false);
          button.removeAttribute("aria-busy");
        }
      }
    },
  };
}
