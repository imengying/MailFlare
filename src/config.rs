//! 运行时配置：`[vars]` / secrets 的读取，以及 `configs` 表的读写。
//!
//! 设计要点：
//! * 静态配置（密码、JWT 密钥、Turnstile 密钥）来自 Worker 环境变量，从不落库；
//! * 动态配置（TG/Bark 开关与 Token）存在 D1 的 `configs` 表，用 key/value 扁平存放，
//!   避免每加一个字段就改表结构。

use serde::{Deserialize, Serialize};
use worker::{D1Database, Env};

use crate::error::{AppError, AppResult};
use crate::util::now_secs;

/// `configs` 表里的 key 常量，集中管理避免拼写错误。
pub mod keys {
    /// 单行 JSON，存放完整的推送配置（见 `Settings`）。
    pub const NOTIFY: &str = "notify";
    /// 登录失败计数，用于实现简单的暴力破解限流。
    pub const LOGIN_GUARD: &str = "login_guard";
}

/// 从 `Env` 读取一个必需字符串（vars 或 secret 均可）。
fn require(env: &Env, name: &str) -> AppResult<String> {
    match env.var(name) {
        Ok(v) => {
            let s = v.to_string();
            if s.is_empty() {
                Err(AppError::Internal(format!(
                    "环境变量 {name} 未配置（请在 wrangler.toml 的 [vars] 或 `wrangler secret put {name}` 中设置）"
                )))
            } else {
                Ok(s)
            }
        }
        Err(_) => Err(AppError::Internal(format!(
            "缺少环境变量 {name}（请在 wrangler.toml 的 [vars] 或 `wrangler secret put {name}` 中设置）"
        ))),
    }
}

/// 读取可选字符串；缺失或为空都返回 `None`。
fn optional(env: &Env, name: &str) -> Option<String> {
    env.var(name)
        .ok()
        .map(|v| v.to_string())
        .filter(|s| !s.is_empty())
}

/// 静态配置快照（每请求构建一次，成本只是几次 JS 属性读取）。
///
/// 注意 `TURNSTILE_SITE_KEY` 与 `ALLOWED_ORIGIN` 不在这里：它们会被
/// `/api/health`、`/api/auth/config`、CORS 预检使用，而这些端点在「密钥尚未
/// 配好」时也必须能工作，所以由 `api::dispatch` 直接从 `Env` 读取。
pub struct Config {
    /// 管理员密码（明文比较，通过 SHA-256 + 常量时间比较）。
    pub admin_password: String,
    /// JWT HMAC-SHA256 签名密钥。
    pub jwt_secret: String,
    /// token 有效期（秒）。
    pub jwt_ttl_secs: i64,
    /// Turnstile secret key；留空表示跳过人机校验（便于本地开发）。
    pub turnstile_secret: Option<String>,
    /// Turnstile 期望的 hostname（可选加固项）。
    pub turnstile_hostname: Option<String>,
    /// 对外访问地址，用于在推送消息里拼「查看详情」链接。
    pub public_url: Option<String>,
    /// 原始 .eml 是否存 R2。
    pub store_raw_eml: bool,
    /// 单封邮件正文入库上限（字节）。
    pub max_body_bytes: usize,
    /// 单封邮件总大小上限（字节）；超过则拒收，避免打爆 128MB 内存。
    pub max_message_bytes: usize,
    /// 单个附件转存 R2 的上限（字节）。
    pub max_attachment_bytes: usize,
    /// 批量写 R2 的并发度。
    pub r2_concurrency: usize,
    /// 列表页默认/最大分页大小。
    pub page_size_default: i64,
    pub page_size_max: i64,
}

impl Config {
    /// 从 `Env` 读取配置。缺失关键项时返回 `Internal` 错误（会以 500 暴露给前端）。
    pub fn from_env(env: &Env) -> AppResult<Self> {
        // 数字型变量容错：解析失败就用默认值，不让整个 Worker 起不来。
        let num = |name: &str, default: i64, min: i64, max: i64| -> i64 {
            optional(env, name)
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(default)
                .clamp(min, max)
        };

        Ok(Self {
            admin_password: require(env, "ADMIN_PASSWORD")?,
            jwt_secret: require(env, "JWT_SECRET")?,
            jwt_ttl_secs: num("JWT_TTL_SECS", 7 * 24 * 3600, 300, 365 * 24 * 3600),
            turnstile_secret: optional(env, "TURNSTILE_SECRET"),
            turnstile_hostname: optional(env, "TURNSTILE_HOSTNAME"),
            // 优先用 PUBLIC_URL；没配就退回 ALLOWED_ORIGIN（同源部署时两者等价）。
            public_url: optional(env, "PUBLIC_URL").or_else(|| optional(env, "ALLOWED_ORIGIN")),
            store_raw_eml: optional(env, "STORE_RAW_EML")
                .map(|v| !matches!(v.as_str(), "0" | "false" | "off"))
                .unwrap_or(true),
            // 正文 256KB 足够展示长邮件，超出部分截断并标注。
            max_body_bytes: num("MAX_BODY_BYTES", 256 * 1024, 4 * 1024, 900 * 1024) as usize,
            // Email Routing 单封邮件上限约 25MB，这里默认 20MB。
            max_message_bytes: num("MAX_MESSAGE_BYTES", 20 * 1024 * 1024, 64 * 1024, 25 * 1024 * 1024)
                as usize,
            max_attachment_bytes: num("MAX_ATTACHMENT_BYTES", 10 * 1024 * 1024, 1024, 25 * 1024 * 1024)
                as usize,
            r2_concurrency: num("R2_CONCURRENCY", 4, 1, 16) as usize,
            page_size_default: num("PAGE_SIZE_DEFAULT", 20, 1, 100),
            page_size_max: num("PAGE_SIZE_MAX", 100, 1, 200),
        })
    }
}

/// 推送设置，整体以一条 JSON 存在 `configs.value` 里。
///
/// 注意：`Serialize` 会输出到前端，所以 Token 用 `#[serde(skip_serializing_if)]`
/// 之外还得做一次“脱敏输出”，见 `Settings::redacted`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub telegram_enabled: bool,
    #[serde(default)]
    pub telegram_bot_token: String,
    #[serde(default)]
    pub telegram_chat_id: String,
    #[serde(default)]
    pub bark_enabled: bool,
    #[serde(default)]
    pub bark_server: String,
    #[serde(default)]
    pub bark_device_key: String,
}

/// 对外的脱敏视图：Token 只回传“是否已设置”，不把明文回给前端。
#[derive(Serialize)]
pub struct SettingsView<'a> {
    pub telegram_enabled: bool,
    pub telegram_bot_token_set: bool,
    pub telegram_bot_token_hint: &'a str,
    pub telegram_chat_id: &'a str,
    pub bark_enabled: bool,
    pub bark_server: &'a str,
    pub bark_device_key_set: bool,
    pub bark_device_key_hint: &'a str,
}

/// 用一个固定规则展示 token 尾部，方便用户在设置页确认填的是哪一份。
fn hint(secret: &str) -> &str {
    if secret.is_empty() {
        ""
    } else if secret.len() <= 8 {
        "****"
    } else {
        // 取最后 4 个**字符**（不是字节），避免切坏多字节字符。
        match secret.char_indices().rev().nth(3) {
            Some((idx, _)) => &secret[idx..],
            None => "****",
        }
    }
}

impl Settings {
    pub fn redacted(&self) -> SettingsView<'_> {
        SettingsView {
            telegram_enabled: self.telegram_enabled,
            telegram_bot_token_set: !self.telegram_bot_token.is_empty(),
            telegram_bot_token_hint: hint(&self.telegram_bot_token),
            telegram_chat_id: &self.telegram_chat_id,
            bark_enabled: self.bark_enabled,
            bark_server: &self.bark_server,
            bark_device_key_set: !self.bark_device_key.is_empty(),
            bark_device_key_hint: hint(&self.bark_device_key),
        }
    }

    /// 写库前归一化：去掉首尾空格、去掉 URL 尾部斜杠。
    pub fn normalized(mut self) -> Self {
        self.telegram_bot_token = self.telegram_bot_token.trim().to_string();
        self.telegram_chat_id = self.telegram_chat_id.trim().to_string();
        self.bark_device_key = self.bark_device_key.trim().to_string();
        self.bark_server = self.bark_server.trim().trim_end_matches('/').to_string();
        if self.bark_server.is_empty() {
            self.bark_server = "https://api.day.app".to_string();
        }
        self
    }
}

/// `configs` 行的反序列化目标。
///
/// 用结构体而非 `first::<String>(Some("value"))`：`value` 列允许 NULL，
/// 直接反序列化成 `String` 会报错；而且行不存在时也要能区分出来。
#[derive(serde::Deserialize)]
struct ConfigRow {
    #[serde(default)]
    value: Option<String>,
}

/// 读取 `configs` 表里指定 key 的 value（不存在或为 NULL 都返回 `None`）。
async fn get_raw(db: &D1Database, key: &str) -> AppResult<Option<String>> {
    let stmt = db
        .prepare("SELECT value FROM configs WHERE key = ?1")
        .bind_refs(&[worker::D1Type::Text(key)])?;
    let row: Option<ConfigRow> = stmt.first(None).await?;
    Ok(row.and_then(|r| r.value))
}

/// UPSERT 一个 key/value。
async fn put_raw(db: &D1Database, key: &str, value: &str) -> AppResult<()> {
    // `updated_at` 用 Real + CAST 绑定：`D1Type::Integer` 只有 i32，
    // 秒级时间戳虽然到 2038 年才溢出，但统一走 f64 更安全。
    let ts = now_secs() as f64;
    db.prepare(
        "INSERT INTO configs (key, value, updated_at) VALUES (?1, ?2, CAST(?3 AS INTEGER)) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind_refs(&[
        worker::D1Type::Text(key),
        worker::D1Type::Text(value),
        worker::D1Type::Real(ts),
    ])?
    .run()
    .await?;
    Ok(())
}

/// 读取推送设置；没有记录时返回全空的默认值。
pub async fn load_settings(db: &D1Database) -> AppResult<Settings> {
    let Some(raw) = get_raw(db, keys::NOTIFY).await? else {
        return Ok(Settings::default());
    };
    // 配置被人工改坏时不应让整个设置页 500，退回默认值并记一条日志。
    Ok(serde_json::from_str(&raw).unwrap_or_else(|e| {
        worker::console_error!("[mailflare] notify 配置解析失败，使用默认值: {e}");
        Settings::default()
    }))
}

/// 覆盖写入推送设置。
pub async fn save_settings(db: &D1Database, settings: &Settings) -> AppResult<()> {
    let raw = serde_json::to_string(settings)?;
    put_raw(db, keys::NOTIFY, &raw).await
}

/// 登录失败计数：`{ "fails": n, "until": unix_secs }`。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct LoginGuard {
    #[serde(default)]
    pub fails: u32,
    /// 锁定截止时间戳（秒）；0 表示未锁定。
    #[serde(default)]
    pub until: i64,
}

impl LoginGuard {
    pub async fn load(db: &D1Database) -> AppResult<Self> {
        let Some(raw) = get_raw(db, keys::LOGIN_GUARD).await? else {
            return Ok(Self::default());
        };
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    pub async fn save(&self, db: &D1Database) -> AppResult<()> {
        put_raw(db, keys::LOGIN_GUARD, &serde_json::to_string(self)?).await
    }
}
