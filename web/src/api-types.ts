/**
 * 与 Rust 后端 API 契约一一对应的类型定义。
 *
 * 这里的字段必须和后端 `src/db.rs` / `src/api.rs` 里的 `Serialize` 结构体
 * 保持同步。改动任何一边都要同步另一边——TypeScript 只能保证前端内部
 * 一致，跨语言契约仍需人工维护（见 README「前后端契约」）。
 */

/** 后端统一成功响应包装：`{"ok":true,"data":...}`。 */
export interface OkEnvelope<T> {
  readonly ok: true;
  readonly data: T;
}

/** 后端统一错误响应：`{"ok":false,"error":"..."}`。 */
export interface ErrEnvelope {
  readonly ok: false;
  readonly error: string;
}

/** 附件元数据，对应 Rust 的 `AttachmentMeta`。 */
export interface AttachmentMeta {
  /** MIME 部件序号，同时是附件下载 ID 的一部分。 */
  readonly index: number;
  readonly filename: string;
  readonly content_type: string;
  /** 解码后的字节数。 */
  readonly size: number;
  /** R2 对象键。 */
  readonly key: string;
  /** 是否为正文内联资源（cid: 图片等）。 */
  readonly inline: boolean;
  /** 规范化后的 Content-ID。 */
  readonly cid?: string;
  /** 是否成功写入 R2。false 时前端显示「未保存」。 */
  readonly stored: boolean;
}

/** 列表项，对应 Rust 的 `EmailSummary`。 */
export interface EmailSummary {
  readonly id: string;
  readonly sender: string;
  /** 收件地址；Catch-All 场景用于区分不同别名。 */
  readonly recipient: string;
  readonly subject: string;
  /** SQL 侧已截取+去标签的正文预览。 */
  readonly preview: string;
  readonly is_read: boolean;
  /** 毫秒 Unix 时间戳。 */
  readonly created_at: number;
  /** 真实文件数（不含内联图片）。 */
  readonly attachment_count: number;
}

/** 列表响应，对应 Rust 的 `ListResponse`。 */
export interface EmailList {
  readonly items: EmailSummary[];
  /** 下一页游标；为 null 表示已到末页。 */
  readonly next_cursor: string | null;
  /** 未读总数。 */
  readonly unread: number;
}

/** 附件下载地址，对应 Rust 的 `AttachmentUrl`。 */
export interface AttachmentUrl {
  readonly index: number;
  readonly url: string;
}

/**
 * 详情响应，对应 Rust 的 `EmailDetailResponse`。
 *
 * 后端用 `#[serde(flatten)]` 把 `EmailDetail` 的字段平铺到顶层，
 * 所以这里是交叉类型而不是嵌套对象。
 */
export type EmailDetail = {
  readonly id: string;
  readonly message_id: string | null;
  readonly sender: string;
  readonly recipient: string;
  readonly subject: string;
  readonly text_body: string;
  readonly html_body: string;
  readonly attachments: AttachmentMeta[];
  readonly is_read: boolean;
  readonly created_at: number;
  readonly attachment_urls: AttachmentUrl[];
};

/** `POST /api/auth/login` 的响应。 */
export interface LoginResult {
  readonly token: string;
  readonly expires_in: number;
}

/** `GET /api/auth/session` 的响应。 */
export interface SessionResult {
  readonly authenticated: boolean;
  readonly expires_in: number;
}

/** `GET /api/auth/config` 的响应。 */
export interface AuthConfig {
  /** Turnstile Site Key；空串表示未启用。 */
  readonly turnstile_site_key: string;
  /** 后端是否配了 Secret（配了才需要前端拿 token）。 */
  readonly turnstile_required: boolean;
  readonly site_name: string;
  readonly site_logo: string;
  /** 已归一成 CSS 值：颜色或 `url("...")`。 */
  readonly site_background: string;
}

/**
 * 后台设置（脱敏视图），对应 Rust 的 `SettingsView`。
 *
 * 后端**永不**返回密钥明文，只回传 `*_set` 与 `*_hint`。
 */
export interface SettingsView {
  readonly site_name: string;
  readonly site_logo: string;
  /** 仅支持 https 图片地址；空串表示默认背景。 */
  readonly site_background: string;
  readonly public_url: string;
  readonly turnstile_site_key: string;
  readonly turnstile_secret_set: boolean;
  readonly turnstile_secret_hint: string;
  readonly telegram_enabled: boolean;
  /** 从邮件提取验证码并置顶显示。 */
  readonly otp_enabled: boolean;
  readonly telegram_bot_token_set: boolean;
  readonly telegram_bot_token_hint: string;
  readonly telegram_chat_id: string;
  readonly bark_enabled: boolean;
  readonly bark_server: string;
  readonly bark_device_key_set: boolean;
  readonly bark_device_key_hint: string;
  // ---- R2（S3 API）----
  readonly r2_account_id: string;
  readonly r2_access_key_id_set: boolean;
  readonly r2_access_key_id_hint: string;
  readonly r2_secret_access_key_set: boolean;
  readonly r2_secret_access_key_hint: string;
  readonly r2_bucket: string;
  /** 发件人黑名单：`a@b.com` / `@b.com` / `b.com` 三种写法。 */
  readonly blocked_senders: readonly string[];
  /** 收件人白名单；为空数组表示接收本域任意地址（catch-all）。 */
  readonly allowed_recipients: readonly string[];
  readonly max_body_bytes: number;
  readonly max_message_bytes: number;
  readonly max_attachment_bytes: number;
  readonly r2_concurrency: number;
  readonly page_size_default: number;
  readonly page_size_max: number;
}

/**
 * `PUT /api/settings` 的请求体。
 *
 * 语义：字段缺省 = 保持不变；密钥字段传空串 = 清空。
 * 后端用 `Option<T>` 区分「没传」与「传了空值」，所以前端只提交
 * 用户真正填了的字段，绝不能把脱敏占位符（含 `*`）回传。
 */
export interface SettingsPatch {
  site_name?: string;
  site_logo?: string;
  site_background?: string;
  public_url?: string;
  turnstile_site_key?: string;
  turnstile_secret?: string;
  telegram_enabled?: boolean;
  otp_enabled?: boolean;
  telegram_bot_token?: string;
  telegram_chat_id?: string;
  bark_enabled?: boolean;
  bark_server?: string;
  bark_device_key?: string;
  r2_account_id?: string;
  r2_access_key_id?: string;
  r2_secret_access_key?: string;
  r2_bucket?: string;
  blocked_senders?: string[];
  allowed_recipients?: string[];
  max_body_bytes?: number;
  max_message_bytes?: number;
  max_attachment_bytes?: number;
  r2_concurrency?: number;
  page_size_default?: number;
  page_size_max?: number;
}
