//! 鉴权：Turnstile 人机校验 + 无状态 JWT（HMAC-SHA256）。
//!
//! 整个系统是单用户，因此 JWT 里不需要角色；只需要区分用途：
//! * `scp = "api"`：登录后访问管理 API；
//! * `scp = "dl"` ：短时效下载票据，可放在 URL query 上（`<img src>` 无法带 header）。

use serde::Deserialize;
use worker::{Fetch, Headers, Method, Request, RequestInit};

use crate::config::{Config, Settings};
use crate::error::{AppError, AppResult};
use crate::jwt::{self, Claims};
use crate::util::{now_secs, secret_eq};

/// API token 用途。
pub const SCOPE_API: &str = "api";
/// 附件下载票据用途。
pub const SCOPE_DL: &str = "dl";

/// 登录态固定有效期：7 天。
pub const TOKEN_TTL_SECS: i64 = 7 * 24 * 3600;

/// Cloudflare Turnstile 校验端点。
const SITEVERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

/// 允许的时钟偏差（秒）：边缘节点之间可能有几百毫秒抖动。
const CLOCK_LEEWAY: i64 = 30;

/// 签发 token。
pub fn issue_token(cfg: &Config, scope: &str, subject: &str, ttl_secs: i64) -> AppResult<String> {
    let now = now_secs();
    let claims = Claims {
        sub: subject.to_string(),
        scp: scope.to_string(),
        iat: now,
        exp: now + ttl_secs,
    };

    jwt::encode(cfg.jwt_secret.as_bytes(), &claims)
        .map_err(|e| AppError::Internal(format!("JWT 签发失败: {e}")))
}

/// 校验 token 并检查用途是否匹配。
pub fn verify_token(cfg: &Config, token: &str, expected_scope: &str) -> AppResult<Claims> {
    let claims = jwt::decode(
        cfg.jwt_secret.as_bytes(),
        token,
        now_secs(),
        CLOCK_LEEWAY,
    )
    .map_err(|_| AppError::Unauthorized("登录已过期，请重新登录".into()))?;

    if claims.scp != expected_scope {
        return Err(AppError::Unauthorized("凭证用途不匹配".into()));
    }
    Ok(claims)
}

/// 从请求里取出 `Authorization: Bearer <token>`。
fn bearer(req: &Request) -> Option<String> {
    let raw = req.headers().get("authorization").ok().flatten()?;
    let raw = raw.trim();
    // 大小写不敏感地剥离 "bearer " 前缀。
    let token = if raw.len() > 7 && raw[..7].eq_ignore_ascii_case("bearer ") {
        &raw[7..]
    } else {
        raw
    };
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// 管理 API 的鉴权守卫：要求请求头里带合法的 `scp=api` token。
pub fn require_admin(req: &Request, cfg: &Config) -> AppResult<()> {
    let token = bearer(req).ok_or_else(|| AppError::Unauthorized("缺少访问凭证".into()))?;
    verify_token(cfg, &token, SCOPE_API)?;
    Ok(())
}

/// 附件鉴权：优先接受 `Authorization` 头，其次接受 `?token=` 下载票据。
///
/// 下载票据必须同时满足：签名有效、未过期、`sub` 等于目标邮件 id。
pub fn require_attachment_access(
    req: &Request,
    url: &url::Url,
    cfg: &Config,
    email_id: &str,
) -> AppResult<()> {
    if let Some(token) = bearer(req) {
        verify_token(cfg, &token, SCOPE_API)?;
        return Ok(());
    }

    let token = crate::util::query_param(url, "token")
        .ok_or_else(|| AppError::Unauthorized("缺少访问凭证".into()))?;
    let claims = verify_token(cfg, &token, SCOPE_DL)?;
    if claims.sub != email_id {
        return Err(AppError::Forbidden("下载票据与邮件不匹配".into()));
    }
    Ok(())
}

/// 校验管理员凭证（用户名 + 密码）。
///
/// 两项都走 SHA-256 + 常量时间比较：即便用户名不匹配也要把密码比较算完，
/// 避免通过响应时间差判断「用户名对不对」。
pub fn verify_credentials(cfg: &Config, username: &str, password: &str) -> bool {
    let user_ok = secret_eq(username, &cfg.admin_username);
    let pass_ok = secret_eq(password, &cfg.admin_password);
    // 用按位与：两个比较都要执行，不做短路。
    user_ok & pass_ok
}

/// Turnstile `siteverify` 的响应体。
#[derive(Debug, Deserialize)]
struct SiteVerifyResponse {
    success: bool,
    #[serde(default)]
    #[serde(rename = "error-codes")]
    error_codes: Vec<String>,
}

/// 校验 Turnstile token。
///
/// 后台设置里没填 Secret 时直接放行（方便本地开发），但会打印警告，
/// 避免线上忘记配置还以为是「正常工作」。
pub async fn verify_turnstile(
    settings: &Settings,
    token: &str,
    remote_ip: Option<&str>,
) -> AppResult<()> {
    if !settings.turnstile_enabled() {
        worker::console_warn!(
            "[mailflare] 未配置 Turnstile Secret，已跳过人机验证（仅建议在本地开发时这样）"
        );
        return Ok(());
    }

    if token.trim().is_empty() {
        return Err(AppError::Forbidden("请先完成人机验证".into()));
    }

    // Turnstile 只接受 x-www-form-urlencoded 或 JSON；这里用表单，兼容性最好。
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("secret", &settings.turnstile_secret);
    serializer.append_pair("response", token);
    // idempotency_key 让 Cloudflare 对重复提交去重，避免用户狂点登录时重复计费/报错。
    serializer.append_pair("idempotency_key", &crate::util::new_id());
    if let Some(ip) = remote_ip {
        serializer.append_pair("remoteip", ip);
    }
    let body = serializer.finish();

    let headers = Headers::new();
    headers.set("content-type", "application/x-www-form-urlencoded")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body)));

    let request = Request::new_with_init(SITEVERIFY_URL, &init)?;
    let mut resp = Fetch::Request(request).send().await?;

    if resp.status_code() != 200 {
        return Err(AppError::Internal(format!(
            "Turnstile 服务返回 HTTP {}",
            resp.status_code()
        )));
    }

    let result: SiteVerifyResponse = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Turnstile 响应解析失败: {e}")))?;

    if !result.success {
        return Err(AppError::Forbidden(format!(
            "人机验证未通过（{}）",
            result.error_codes.join(",")
        )));
    }

    Ok(())
}
