//! 运行时配置。
//!
//! 登录根凭证（`ADMIN_USERNAME` / `ADMIN_PASSWORD` / `JWT_SECRET`）来自
//! 环境变量；其余参数全部存在 D1 的 `configs` 表，网页里可改。

use serde::{Deserialize, Serialize};
use worker::{D1Database, Env};

use crate::error::{AppError, AppResult};
use crate::util::now_secs;

/// `configs` 表里的 key。
pub mod keys {
    pub const SETTINGS: &str = "settings";
    pub const LOGIN_GUARD: &str = "login_guard";
}

/// 读取必需的环境变量，缺失或为空时报错。
fn require(env: &Env, name: &str) -> AppResult<String> {
    let value = env.var(name).map(|v| v.to_string()).unwrap_or_default();
    if value.is_empty() {
        return Err(AppError::Internal(format!(
            "环境变量 {name} 未配置，请在控制台 Settings → Variables and Secrets 中添加"
        )));
    }
    Ok(value)
}

pub struct Config {
    pub admin_username: String,
    pub admin_password: String,
    pub jwt_secret: String,
}

impl Config {
    pub fn from_env(env: &Env) -> AppResult<Self> {
        Ok(Self {
            admin_username: require(env, "ADMIN_USERNAME")?,
            admin_password: require(env, "ADMIN_PASSWORD")?,
            jwt_secret: require(env, "JWT_SECRET")?,
        })
    }
}

// ===========================================================================
// 后台设置（D1）
// ===========================================================================

/// 全部可配置项，整体以一条 JSON 存在 `configs.value` 里。
///
/// 结构体级 `#[serde(default)]` 让缺字段时自动取 [`Default`] 里的值——
/// 以后新增配置项不需要写数据迁移，旧记录也能正常读出来。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // ---- 站点 -----------------------------------------------------------
    /// 推送消息里「查看详情」链接的前缀。
    pub public_url: String,

    // ---- Turnstile ------------------------------------------------------
    /// 公开值，下发给前端渲染挂件。
    pub turnstile_site_key: String,
    /// 留空则跳过人机校验。
    pub turnstile_secret: String,
    /// 限定 token 只能在该 hostname 上使用。
    pub turnstile_hostname: String,

    // ---- Telegram -------------------------------------------------------
    pub telegram_enabled: bool,
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,

    // ---- Bark -----------------------------------------------------------
    pub bark_enabled: bool,
    pub bark_server: String,
    pub bark_device_key: String,

    // ---- R2（S3 API）----
    /// 端点主机名是 `{account_id}.r2.cloudflarestorage.com`。
    pub r2_account_id: String,
    pub r2_access_key_id: String,
    pub r2_secret_access_key: String,
    pub r2_bucket: String,

    // ---- 收信过滤 -------------------------------------------------------
    /// 发件人黑名单，命中即静默丢弃（不退信）。
    pub blocked_senders: Vec<String>,
    /// 收件人白名单。**留空表示不限**，任意地址都收（catch-all）。
    pub allowed_recipients: Vec<String>,

    // ---- 收信 -----------------------------------------------------------
    /// 正文入库上限（字节），超出截断。
    pub max_body_bytes: i64,
    /// 单封邮件上限（字节），超过拒收。
    pub max_message_bytes: i64,
    pub max_attachment_bytes: i64,
    pub r2_concurrency: i64,

    // ---- 列表 -----------------------------------------------------------
    pub page_size_default: i64,
    pub page_size_max: i64,

    // ---- 会话 -----------------------------------------------------------
    /// 登录有效期（秒）。
    pub jwt_ttl_secs: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            public_url: String::new(),
            turnstile_site_key: String::new(),
            turnstile_secret: String::new(),
            turnstile_hostname: String::new(),
            telegram_enabled: false,
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            bark_enabled: false,
            bark_server: "https://api.day.app".to_string(),
            bark_device_key: String::new(),
            r2_account_id: String::new(),
            r2_access_key_id: String::new(),
            r2_secret_access_key: String::new(),
            r2_bucket: String::new(),
            blocked_senders: Vec::new(),
            allowed_recipients: Vec::new(),
            // 256KB 足够展示长邮件；Email Routing 单封上限约 25MB。
            max_body_bytes: 256 * 1024,
            max_message_bytes: 20 * 1024 * 1024,
            max_attachment_bytes: 10 * 1024 * 1024,
            r2_concurrency: 4,
            page_size_default: 20,
            page_size_max: 100,
            jwt_ttl_secs: 7 * 24 * 3600,
        }
    }
}

impl Settings {
    /// 归一化并夹紧取值范围，写库前调用。
    pub fn normalized(mut self) -> Self {
        self.public_url = self.public_url.trim().trim_end_matches('/').to_string();
        self.turnstile_site_key = self.turnstile_site_key.trim().to_string();
        self.turnstile_secret = self.turnstile_secret.trim().to_string();
        self.turnstile_hostname = self.turnstile_hostname.trim().to_string();
        self.telegram_bot_token = self.telegram_bot_token.trim().to_string();
        self.telegram_chat_id = self.telegram_chat_id.trim().to_string();
        self.bark_device_key = self.bark_device_key.trim().to_string();
        self.bark_server = self.bark_server.trim().trim_end_matches('/').to_string();
        if self.bark_server.is_empty() {
            self.bark_server = "https://api.day.app".to_string();
        }

        self.r2_account_id = self.r2_account_id.trim().to_string();
        self.r2_access_key_id = self.r2_access_key_id.trim().to_string();
        self.r2_secret_access_key = self.r2_secret_access_key.trim().to_string();
        self.r2_bucket = self.r2_bucket.trim().trim_matches('/').to_string();

        // 地址列表：统一小写、去空格、丢空项、去重。
        self.blocked_senders = normalize_list(self.blocked_senders);
        self.allowed_recipients = normalize_list(self.allowed_recipients);

        // 4KB 起步（再小连正常邮件都放不下），上限 900KB（D1 单行有长度约束）。
        self.max_body_bytes = self.max_body_bytes.clamp(4 * 1024, 900 * 1024);
        // 64KB ~ 25MB（Email Routing 的硬上限）。
        self.max_message_bytes = self.max_message_bytes.clamp(64 * 1024, 25 * 1024 * 1024);
        self.max_attachment_bytes = self.max_attachment_bytes.clamp(1024, 25 * 1024 * 1024);
        self.r2_concurrency = self.r2_concurrency.clamp(1, 16);
        self.page_size_default = self.page_size_default.clamp(1, 100);
        self.page_size_max = self.page_size_max.clamp(1, 200);
        // 默认页大小不能超过上限，否则第一页就会请求超量。
        if self.page_size_default > self.page_size_max {
            self.page_size_default = self.page_size_max;
        }
        // 5 分钟 ~ 1 年。
        self.jwt_ttl_secs = self.jwt_ttl_secs.clamp(300, 365 * 24 * 3600);

        self
    }

    /// 配了 secret 才算启用。
    pub fn turnstile_enabled(&self) -> bool {
        !self.turnstile_secret.is_empty()
    }

    /// 四项都填了才能用 S3 API。
    pub fn r2_configured(&self) -> bool {
        !self.r2_account_id.is_empty()
            && !self.r2_access_key_id.is_empty()
            && !self.r2_secret_access_key.is_empty()
            && !self.r2_bucket.is_empty()
    }

    /// 空串归一成 `None`。
    pub fn public_url(&self) -> Option<&str> {
        (!self.public_url.is_empty()).then_some(self.public_url.as_str())
    }

    /// 密钥只回传「是否已设置」+ 尾部提示。
    pub fn redacted(&self) -> SettingsView<'_> {
        SettingsView {
            public_url: &self.public_url,
            turnstile_site_key: &self.turnstile_site_key,
            turnstile_secret_set: !self.turnstile_secret.is_empty(),
            turnstile_secret_hint: hint(&self.turnstile_secret),
            turnstile_hostname: &self.turnstile_hostname,
            telegram_enabled: self.telegram_enabled,
            telegram_bot_token_set: !self.telegram_bot_token.is_empty(),
            telegram_bot_token_hint: hint(&self.telegram_bot_token),
            telegram_chat_id: &self.telegram_chat_id,
            bark_enabled: self.bark_enabled,
            bark_server: &self.bark_server,
            bark_device_key_set: !self.bark_device_key.is_empty(),
            bark_device_key_hint: hint(&self.bark_device_key),
            r2_account_id: &self.r2_account_id,
            r2_access_key_id_set: !self.r2_access_key_id.is_empty(),
            r2_access_key_id_hint: hint(&self.r2_access_key_id),
            r2_secret_access_key_set: !self.r2_secret_access_key.is_empty(),
            r2_secret_access_key_hint: hint(&self.r2_secret_access_key),
            r2_bucket: &self.r2_bucket,
            blocked_senders: &self.blocked_senders,
            allowed_recipients: &self.allowed_recipients,
            max_body_bytes: self.max_body_bytes,
            max_message_bytes: self.max_message_bytes,
            max_attachment_bytes: self.max_attachment_bytes,
            r2_concurrency: self.r2_concurrency,
            page_size_default: self.page_size_default,
            page_size_max: self.page_size_max,
            jwt_ttl_secs: self.jwt_ttl_secs,
        }
    }
}

#[derive(Serialize)]
pub struct SettingsView<'a> {
    pub public_url: &'a str,
    pub turnstile_site_key: &'a str,
    pub turnstile_secret_set: bool,
    pub turnstile_secret_hint: &'a str,
    pub turnstile_hostname: &'a str,
    pub telegram_enabled: bool,
    pub telegram_bot_token_set: bool,
    pub telegram_bot_token_hint: &'a str,
    pub telegram_chat_id: &'a str,
    pub bark_enabled: bool,
    pub bark_server: &'a str,
    pub bark_device_key_set: bool,
    pub bark_device_key_hint: &'a str,
    pub r2_account_id: &'a str,
    pub r2_access_key_id_set: bool,
    pub r2_access_key_id_hint: &'a str,
    pub r2_secret_access_key_set: bool,
    pub r2_secret_access_key_hint: &'a str,
    pub r2_bucket: &'a str,
    pub blocked_senders: &'a [String],
    pub allowed_recipients: &'a [String],
    pub max_body_bytes: i64,
    pub max_message_bytes: i64,
    pub max_attachment_bytes: i64,
    pub r2_concurrency: i64,
    pub page_size_default: i64,
    pub page_size_max: i64,
    pub jwt_ttl_secs: i64,
}

/// 去空格、转小写、丢空项、去重。
fn normalize_list(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let value = item.trim().to_lowercase();
        if value.is_empty() || out.contains(&value) {
            continue;
        }
        out.push(value);
    }
    out
}

/// 抽出纯邮箱地址，去掉显示名与尖括号。
///
/// 信封地址通常是裸地址，但 `From` 头常见 `"名字" <a@b.com>`。
pub fn bare_address(raw: &str) -> String {
    let raw = raw.trim();
    // 有尖括号时取最后一对的内容：`"a <b>" <c@d.com>` 也能取对。
    if let (Some(start), Some(end)) = (raw.rfind('<'), raw.rfind('>'))
        && start < end
    {
        return raw[start + 1..end].trim().to_lowercase();
    }
    raw.to_lowercase()
}

/// 地址规则匹配：`a@b.com` 精确、`@b.com` 域名及子域、`b.com` 同 `@b.com`。
fn rule_matches(rule: &str, address: &str, domain: &str) -> bool {
    // 规则也统一小写：`normalized()` 已经转过一次，但有人可能直接改
    // D1 表塞进大写项，这里再兜一次，免得规则静默失效。
    let rule = rule.trim().to_lowercase();
    match rule.strip_prefix('@') {
        Some(suffix) => domain == suffix || domain.ends_with(&format!(".{suffix}")),
        None if rule.contains('@') => address == rule,
        None => domain == rule || domain.ends_with(&format!(".{rule}")),
    }
}

pub fn sender_blocked(blocked: &[String], sender: &str) -> bool {
    if blocked.is_empty() {
        return false;
    }
    let address = bare_address(sender);
    if address.is_empty() {
        return false;
    }
    let domain = address.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
    blocked.iter().any(|rule| rule_matches(rule, &address, domain))
}

/// **白名单为空时一律放行**，这是免注册收件的前提。
pub fn recipient_allowed(allowed: &[String], recipient: &str) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let address = bare_address(recipient);
    if address.is_empty() {
        return false;
    }
    let domain = address.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
    allowed.iter().any(|rule| rule_matches(rule, &address, domain))
}

/// 接受 `a@b.com` / `@b.com` / `b.com`；不允许空格与通配符。
pub fn valid_address_rule(rule: &str) -> bool {
    let rule = rule.trim();
    if rule.is_empty() || rule.len() > 254 {
        return false;
    }
    if rule.contains(char::is_whitespace) || rule.contains('*') {
        return false;
    }
    // 去掉可选的 `@` 前缀后，剩下的必须像一个域名（或一个完整地址）。
    let body = rule.strip_prefix('@').unwrap_or(rule);
    if body.is_empty() {
        return false;
    }
    match body.rsplit_once('@') {
        // 完整地址：域名部分必须含 `.`
        Some((local, domain)) => !local.is_empty() && domain.contains('.') && !domain.is_empty(),
        // 纯域名
        None => body.contains('.') && !body.starts_with('.') && !body.ends_with('.'),
    }
}

/// `None` 保持不变，`Some("")` 清空，`Some(v)` 覆盖。
///
/// 含 `*` 的值忽略，那是脱敏视图（`****abcd`）的特征。
pub fn merge_secret(slot: &mut String, patch: Option<String>) {
    if let Some(v) = patch.filter(|v| !v.contains('*')) {
        *slot = v;
    }
}

fn hint(secret: &str) -> &str {
    if secret.is_empty() {
        ""
    } else if secret.len() <= 8 {
        "****"
    } else {
        // 按**字符**取最后 4 个，避免切坏多字节字符。
        match secret.char_indices().rev().nth(3) {
            Some((idx, _)) => &secret[idx..],
            None => "****",
        }
    }
}

// ===========================================================================
// configs 表读写
// ===========================================================================

/// `value` 列允许 NULL，所以用 `Option` 接；行不存在时也能区分。
#[derive(Deserialize)]
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
    // `D1Type::Integer` 只有 i32，秒级时间戳统一走 Real + CAST。
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

/// 读取设置；没有记录时返回默认值。
pub async fn load_settings(db: &D1Database) -> AppResult<Settings> {
    let Some(raw) = get_raw(db, keys::SETTINGS).await? else {
        return Ok(Settings::default());
    };
    Ok(serde_json::from_str::<Settings>(&raw)
        .map(Settings::normalized)
        .unwrap_or_else(|e| {
            // 配置被人工改坏时不该让整个设置页 500，退回默认值并记一条日志。
            worker::console_error!("[mailflare] settings 解析失败，使用默认值: {e}");
            Settings::default()
        }))
}

/// 覆盖写入设置。
pub async fn save_settings(db: &D1Database, settings: &Settings) -> AppResult<()> {
    put_raw(db, keys::SETTINGS, &serde_json::to_string(settings)?).await
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
