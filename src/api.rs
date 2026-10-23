//! HTTP API 层。
//!
//! 路由表（全部挂在 `/api` 下，命中 `/api/*` 之外的请求回落到静态资源）：
//! | 方法   | 路径                     | 说明                              |
//! |--------|--------------------------|-----------------------------------|
//! | POST   | /api/auth/login          | 密码 + Turnstile 换 JWT           |
//! | POST   | /api/auth/logout         | 纯前端丢弃 token（占位）          |
//! | GET    | /api/auth/session        | 校验当前 token 是否仍有效         |
//! | GET    | /api/emails              | 分页列表（支持未读筛选/游标）     |
//! | GET    | /api/emails/:id          | 详情（自动标记已读）              |
//! | DELETE | /api/emails/:id          | 删除 D1 记录 + R2 附件            |
//! | POST   | /api/emails/:id/read     | 标记已读/未读                     |
//! | GET    | /api/attachments/:id     | 流式下载 R2 附件                  |
//! | GET    | /api/settings            | 读取推送配置（脱敏）              |
//! | PUT    | /api/settings            | 更新推送配置                      |
//! | GET    | /api/emails/:id/download | 下载原始 .eml                     |

use serde::{Deserialize, Serialize};
use worker::{Bucket, D1Database, Env, Request, Response};

use crate::auth::{self, SCOPE_DL};
use crate::config::{self, Config};
use crate::db::{self, ListQuery};
use crate::error::{AppError, AppResult};
use crate::r2;
use crate::util::{ok_json, query_i64, query_param};

/// 附件下载票据有效期（秒）。
const DL_TICKET_TTL: i64 = 600;

/// 构造 API 用的上下文（每请求一次，成本很低）。
pub struct ApiCtx<'a> {
    pub env: &'a Env,
    pub cfg: &'a Config,
}

impl<'a> ApiCtx<'a> {
    fn db(&self) -> AppResult<D1Database> {
        Ok(self.env.d1("DB")?)
    }

    fn bucket(&self) -> AppResult<Bucket> {
        Ok(self.env.bucket("R2")?)
    }
}

/// API 总入口：由 `lib.rs` 在 `/api` 前缀命中后调用。
///
/// 顺带处理 CORS：同源部署时不需要任何额外头；若用户显式配置了
/// `ALLOWED_ORIGIN`（例如前端放 Pages、后端放 Worker），才允许该来源跨域。
pub async fn dispatch(mut req: Request, env: Env, path: &str) -> Response {
    // `ALLOWED_ORIGIN` 读一次即可，预检和实际响应都要用。
    let allowed_origin = env
        .var("ALLOWED_ORIGIN")
        .ok()
        .map(|v| v.to_string())
        .filter(|s| !s.is_empty());

    // 预检请求：只回 CORS 头，不执行业务逻辑，也不消耗 D1 配额。
    if req.method() == worker::Method::Options {
        return preflight(&req, allowed_origin.as_deref());
    }

    // ---- 不依赖完整配置的端点 ------------------------------------------
    // 健康检查/前端引导必须能在「还没配好密钥」时工作，否则部署后自检会
    // 直接 500，反而看不出问题出在哪。
    let mut resp = match path {
        "/api/health" => ok_json(&serde_json::json!({
            "service": "mailflare",
            "version": env!("CARGO_PKG_VERSION"),
        })),
        "/api/auth/config" => {
            if req.method() != worker::Method::Get {
                method_not_allowed().into_response()
            } else {
                let site_key = env
                    .var("TURNSTILE_SITE_KEY")
                    .ok()
                    .map(|v| v.to_string())
                    .filter(|s| !s.is_empty());
                let secret_set = env
                    .var("TURNSTILE_SECRET")
                    .ok()
                    .map(|v| !v.to_string().is_empty())
                    .unwrap_or(false);
                ok_json(&AuthConfigResponse {
                    turnstile_site_key: site_key,
                    turnstile_required: secret_set,
                })
            }
        }
        _ => {
            // 其余接口都需要完整配置。
            let cfg = match Config::from_env(&env) {
                Ok(c) => c,
                Err(e) => return e.into_response(),
            };
            let ctx = ApiCtx {
                env: &env,
                cfg: &cfg,
            };
            match route(&mut req, &ctx, path).await {
                Ok(r) => r,
                Err(e) => e.into_response(),
            }
        }
    };

    if let Some(origin) = cors_origin(&req, allowed_origin.as_deref()) {
        let headers = resp.headers_mut();
        let _ = headers.set("access-control-allow-origin", &origin);
        // 让缓存/CDN 知道响应随 Origin 变化。
        let _ = headers.append("vary", "origin");
    }

    resp
}

/// 判断本次请求是否应该带上 CORS 头，返回要回显的 Origin。
///
/// 只有在显式配置了 `ALLOWED_ORIGIN` 且与请求来源**完全一致**时才放行。
/// 没配置就一个 CORS 头都不发：同源部署本来不需要，而「无脑回显 Origin」
/// 会让任意站点都能读你的邮件列表。
fn cors_origin(req: &Request, allowed: Option<&str>) -> Option<String> {
    let expected = allowed?;
    let origin = req.headers().get("origin").ok().flatten()?;
    expected
        .eq_ignore_ascii_case(origin.trim())
        .then_some(origin)
}

/// 处理 `OPTIONS` 预检。
fn preflight(req: &Request, allowed: Option<&str>) -> Response {
    let headers = worker::Headers::new();

    if let Some(origin) = cors_origin(req, allowed) {
        let _ = headers.set("access-control-allow-origin", &origin);
        let _ = headers.set("access-control-allow-methods", "GET, POST, PUT, DELETE, OPTIONS");
        let _ = headers.set(
            "access-control-allow-headers",
            "authorization, content-type, range",
        );
        let _ = headers.set("access-control-max-age", "86400");
        let _ = headers.append("vary", "origin");
    }

    // `ResponseBuilder::empty()` 不返回 Result，天然不会 panic。
    worker::ResponseBuilder::new()
        .with_headers(headers)
        .with_status(204)
        .empty()
}

/// 静态路由分发表。
///
/// 刻意不用 `Router` 的 `data` 泛型：我们的 handler 需要 `Request` 的**可变**
/// 借用（读 body），而 `Router` 会把 `Request` move 进 handler，最后还是得
/// 重新组织所有权；手写分发更直观，也少一层 `Rc<dyn Fn>` 间接调用。
async fn route(req: &mut Request, ctx: &ApiCtx<'_>, path: &str) -> AppResult<Response> {
    let method = req.method();

    // ---- 认证相关（无需登录）------------------------------------------
    match path {
        "/api/auth/login" => {
            require_method(&method, worker::Method::Post)?;
            return login(req, ctx).await;
        }
        // 无状态 JWT：注销只是前端丢弃 token。保留这个端点是为了让前端
        // 有一处统一的地方记录「已退出」，也方便将来加 token 黑名单。
        "/api/auth/logout" => {
            require_method(&method, worker::Method::Post)?;
            return Ok(ok_json(&serde_json::json!({ "ok": true })));
        }
        "/api/auth/session" => {
            require_method(&method, worker::Method::Get)?;
            auth::require_admin(req, ctx.cfg)?;
            return Ok(ok_json(&SessionResponse {
                authenticated: true,
                expires_in: ctx.cfg.jwt_ttl_secs,
            }));
        }
        _ => {}
    }

    // ---- 文件下载：独立鉴权 --------------------------------------------
    // 这两条路由必须放在全局登录守卫**之前**。原因是它们要能被浏览器直接
    // 打开（`<img src>` / `<a download>`），而这类请求无法携带
    // Authorization 头，只能依赖 URL 上的短时效下载票据（`?token=`）。
    // 具体校验在各自 handler 里完成（票据 / Bearer 二选一）。
    if let Some(object_id) = path.strip_prefix("/api/attachments/") {
        require_method(&method, worker::Method::Get)?;
        if object_id.is_empty() || object_id.contains('/') {
            return Err(AppError::NotFound);
        }
        return get_attachment(req, ctx, object_id).await;
    }
    if let Some(rest) = path.strip_prefix("/api/emails/")
        && let Some(id) = rest.strip_suffix("/download")
        && !id.is_empty()
        && !id.contains('/')
    {
        require_method(&method, worker::Method::Get)?;
        return download_raw(req, ctx, id).await;
    }

    // ---- 以下接口都需要登录 --------------------------------------------
    auth::require_admin(req, ctx.cfg)?;

    match path {
        "/api/emails" => {
            require_method(&method, worker::Method::Get)?;
            list_emails(req, ctx).await
        }
        // 批量操作：必须放在 `/api/emails/{id}` 通配之前，
        // 否则会被当成 id 为 `read-all` 的单封邮件查询。
        "/api/emails/read-all" => {
            require_method(&method, worker::Method::Post)?;
            let db = ctx.db()?;
            let changed = db::mark_all_read(&db).await?;
            Ok(ok_json(&serde_json::json!({ "updated": changed })))
        }
        "/api/settings" => match method {
            worker::Method::Get => get_settings(ctx).await,
            worker::Method::Put => put_settings(req, ctx).await,
            _ => Err(method_not_allowed()),
        },
        _ => {
            // ---- 带 :id 的路由 -----------------------------------------
            if let Some(rest) = path.strip_prefix("/api/emails/") {
                // `/api/emails/{id}` 或 `/api/emails/{id}/read` 或 `.../download`
                let mut parts = rest.splitn(2, '/');
                let id = parts.next().unwrap_or_default();
                let action = parts.next();
                if id.is_empty() {
                    return Err(AppError::NotFound);
                }
                return match (action, method) {
                    (None, worker::Method::Get) => get_email(ctx, id).await,
                    (None, worker::Method::Delete) => delete_email(ctx, id).await,
                    (Some("read"), worker::Method::Post) => mark_read(req, ctx, id).await,
                    (None | Some(_), _) => Err(method_not_allowed()),
                };
            }

            Err(AppError::NotFound)
        }
    }
}

/// 方法不匹配 -> 405。
fn method_not_allowed() -> AppError {
    AppError::MethodNotAllowed("请求方法不被支持".into())
}

fn require_method(actual: &worker::Method, expected: worker::Method) -> AppResult<()> {
    if *actual == expected {
        Ok(())
    } else {
        Err(method_not_allowed())
    }
}

// ===========================================================================
// 认证
// ===========================================================================

#[derive(Deserialize)]
struct LoginRequest {
    password: String,
    /// Turnstile token；未配置 secret 时可为空。
    #[serde(default)]
    turnstile_token: String,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
    expires_in: i64,
}

#[derive(Serialize)]
struct AuthConfigResponse {
    turnstile_site_key: Option<String>,
    turnstile_required: bool,
}

#[derive(Serialize)]
struct SessionResponse {
    authenticated: bool,
    expires_in: i64,
}

/// 登录：Turnstile -> 密码 -> 签发 JWT。
///
/// 附带一个简单的失败限流：连续 5 次失败锁定 5 分钟，状态存在 `configs` 表里，
/// 不依赖 Durable Object 也能挡住常见的密码爆破。
async fn login(req: &mut Request, ctx: &ApiCtx<'_>) -> AppResult<Response> {
    let body: LoginRequest = req
        .json()
        .await
        .map_err(|_| AppError::BadRequest("请求体必须是 JSON".into()))?;

    let db = ctx.db()?;

    // ---- 0. 先看是否处于锁定期 -----------------------------------------
    let mut guard = config::LoginGuard::load(&db).await?;
    let now = crate::util::now_secs();
    if guard.until > now {
        let wait = guard.until - now;
        return Err(AppError::Forbidden(format!(
            "尝试次数过多，请在 {wait} 秒后重试"
        )));
    }

    // ---- 1. Turnstile --------------------------------------------------
    // client IP 由 Cloudflare 注入；本地 dev 时可能是 None。
    let client_ip = req.headers().get("cf-connecting-ip").ok().flatten();
    if let Err(e) = auth::verify_turnstile(ctx.cfg, &body.turnstile_token, client_ip.as_deref()).await
    {
        // Turnstile 失败也计入失败次数，防止刷接口。
        record_failure(&db, &mut guard, now).await?;
        return Err(e);
    }

    // ---- 2. 密码 -------------------------------------------------------
    if !auth::verify_password(ctx.cfg, &body.password) {
        record_failure(&db, &mut guard, now).await?;
        return Err(AppError::Forbidden("密码错误".into()));
    }

    // ---- 3. 成功：清空计数并签发 token ---------------------------------
    guard = config::LoginGuard::default();
    guard.save(&db).await?;

    let token = auth::issue_token(ctx.cfg, auth::SCOPE_API, "admin", ctx.cfg.jwt_ttl_secs)?;
    Ok(ok_json(&LoginResponse {
        token,
        expires_in: ctx.cfg.jwt_ttl_secs,
    }))
}

/// 累加失败次数，达到阈值就锁定 5 分钟。
async fn record_failure(
    db: &D1Database,
    guard: &mut config::LoginGuard,
    now: i64,
) -> AppResult<()> {
    guard.fails = guard.fails.saturating_add(1);
    if guard.fails >= 5 {
        guard.until = now + 300;
        guard.fails = 0;
    }
    guard.save(db).await
}

// ===========================================================================
// 邮件列表 / 详情 / 删除
// ===========================================================================

#[derive(Serialize)]
struct ListResponse {
    items: Vec<db::EmailSummary>,
    next_cursor: Option<String>,
    unread: i64,
}

/// 解析游标 `"{created_at}_{id}"`。
fn parse_cursor(raw: &str) -> Option<(i64, String)> {
    let (ts, id) = raw.rsplit_once('_')?;
    Some((ts.parse().ok()?, id.to_string()))
}

async fn list_emails(req: &Request, ctx: &ApiCtx<'_>) -> AppResult<Response> {
    let url = req.url()?;

    let unread_only = matches!(
        query_param(&url, "filter").as_deref(),
        Some("unread") | Some("unseen")
    ) || matches!(query_param(&url, "unread").as_deref(), Some("1") | Some("true"));

    let limit = query_i64(
        &url,
        "limit",
        ctx.cfg.page_size_default,
        1,
        ctx.cfg.page_size_max,
    );

    let cursor = query_param(&url, "cursor").and_then(|c| parse_cursor(&c));

    let db = ctx.db()?;
    let (items, next_cursor, unread) = db::list_emails(
        &db,
        &ListQuery {
            unread_only,
            cursor,
            limit,
        },
    )
    .await?;

    Ok(ok_json(&ListResponse {
        items,
        next_cursor,
        unread,
    }))
}

/// 详情响应：在 DB 记录之上补充 R2 附件下载链接与 HTML 里 `cid:` 的重写。
#[derive(Serialize)]
struct EmailDetailResponse {
    #[serde(flatten)]
    email: db::EmailDetail,
    /// 每个可下载附件的 URL（只包含成功转存到 R2 的）。
    attachment_urls: Vec<AttachmentUrl>,
    /// 原始 `.eml` 的下载地址；未开启原文保存时为 `null`。
    raw_url: Option<String>,
}

#[derive(Serialize)]
struct AttachmentUrl {
    index: usize,
    /// 相对路径；前端按需拼接 origin。
    url: String,
}

async fn get_email(ctx: &ApiCtx<'_>, id: &str) -> AppResult<Response> {
    let db = ctx.db()?;
    let Some(mut email) = db::get_email(&db, id).await? else {
        return Err(AppError::NotFound);
    };

    // 打开详情即标记已读（只写一次，避免每次 GET 都产生写放大）。
    if !email.is_read {
        db::set_read(&db, id, true).await?;
        email.is_read = true;
    }

    // 生成下载票据（短时效，可放进 <img src="...?token=">）。
    // 有效期 10 分钟：足够用户读完一封邮件，又不会让 URL 被长期泄露可用。
    let dl_token = auth::issue_token(ctx.cfg, SCOPE_DL, id, DL_TICKET_TTL)?;

    let attachment_urls = email
        .attachments
        .iter()
        .filter(|a| a.stored)
        .map(|a| AttachmentUrl {
            index: a.index,
            url: format!(
                "/api/attachments/{}?token={}",
                r2::attachment_object_id(id, a.index),
                dl_token
            ),
        })
        .collect();

    // 把 HTML 正文里的 `cid:xxx` 换成附件 URL，内嵌图片就能直接显示。
    if !email.html_body.is_empty() {
        email.html_body = rewrite_cid(&email.html_body, id, &email.attachments, &dl_token);
    }

    let raw_url = ctx
        .cfg
        .store_raw_eml
        .then(|| format!("/api/emails/{id}/download?token={dl_token}"));

    Ok(ok_json(&EmailDetailResponse {
        email,
        attachment_urls,
        raw_url,
    }))
}

/// 仅供测试：构造一个带内联图片的附件表。
#[cfg(test)]
pub fn rewrite_cid_for_test(html: &str, email_id: &str, token: &str) -> String {
    let metas = [db::AttachmentMeta {
        index: 0,
        filename: "logo.png".into(),
        content_type: "image/png".into(),
        size: 4,
        key: "att/e1/0".into(),
        inline: true,
        cid: Some("logo@example".into()),
        stored: true,
    }];
    rewrite_cid(html, email_id, &metas, token)
}

/// 把 HTML 正文里的 `cid:xxx` 替换成可访问的附件地址。
///
/// 实现是**单遍扫描**：逐封邮件做 N 次全串 `replace` 的时间复杂度是
/// O(N*len)，在正文较大时既慢又会产生大量临时字符串。这里只在遇到
/// `cid:` 时才取一次子串匹配，正文不含内联图片时几乎是纯 memchr 扫描。
fn rewrite_cid(
    html: &str,
    email_id: &str,
    attachments: &[db::AttachmentMeta],
    token: &str,
) -> String {
    if !html.contains("cid:") {
        // 绝大多数邮件没有内联资源，直接返回原串（零分配）。
        return html.to_string();
    }

    // cid -> 附件 URL 的查找表；同时收录原始与百分号编码两种写法。
    let mut table: Vec<(String, String)> = Vec::with_capacity(attachments.len() * 2);
    for att in attachments {
        let (Some(cid), true) = (att.cid.as_deref(), att.stored) else {
            continue;
        };
        let url = format!(
            "/api/attachments/{}?token={}",
            r2::attachment_object_id(email_id, att.index),
            token
        );
        table.push((cid.to_string(), url.clone()));
        let encoded = crate::util::percent_encode(cid);
        if encoded != cid {
            table.push((encoded, url));
        }
    }
    if table.is_empty() {
        return html.to_string();
    }

    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find("cid:") {
        let (before, after) = rest.split_at(pos);
        out.push_str(before);
        let after_colon = &after[4..];

        match table
            .iter()
            .find(|(cid, _)| after_colon.starts_with(cid.as_str()))
        {
            Some((cid, url)) => {
                out.push_str(url);
                rest = &after_colon[cid.len()..];
            }
            // `cid:` 出现在普通文本里（而非 URL），原样保留。
            None => {
                out.push_str("cid:");
                rest = after_colon;
            }
        }
    }
    out.push_str(rest);
    out
}

async fn mark_read(req: &mut Request, ctx: &ApiCtx<'_>, id: &str) -> AppResult<Response> {
    // body 可选：`{"is_read": false}` 用来标回未读；缺省表示标记已读。
    #[derive(Deserialize, Default)]
    struct Body {
        #[serde(default)]
        is_read: Option<bool>,
    }
    let body: Body = req.json().await.unwrap_or_default();
    let read = body.is_read.unwrap_or(true);

    let db = ctx.db()?;
    if !db::set_read(&db, id, read).await? {
        return Err(AppError::NotFound);
    }
    Ok(ok_json(&serde_json::json!({ "id": id, "is_read": read })))
}

async fn delete_email(ctx: &ApiCtx<'_>, id: &str) -> AppResult<Response> {
    let db = ctx.db()?;

    // 先删 D1：即使后续 R2 清理失败，也不会出现“列表里还有、点开 404”的鬼数据。
    let Some(attachments) = db::delete_email(&db, id).await? else {
        return Err(AppError::NotFound);
    };

    // R2 删除失败不阻塞响应，孤对象可由 bucket 生命周期规则兜底清理。
    let bucket = ctx.bucket()?;
    r2::delete_email_objects(&bucket, id, &attachments, ctx.cfg.store_raw_eml).await;

    Ok(ok_json(&serde_json::json!({ "id": id, "deleted": true })))
}

// ===========================================================================
// 附件下载 / 原始邮件下载
// ===========================================================================

/// 流式读取 R2 附件。
///
/// 用 `ResponseBody::Stream` 把 R2 的 `ReadableStream` 直接交给运行时转发：
/// 附件字节**不经过 WASM 堆**，既不会触发 128MB 内存上限，也不消耗 CPU 时间。
async fn get_attachment(req: &Request, ctx: &ApiCtx<'_>, object_id: &str) -> AppResult<Response> {
    let (email_id, index) = r2::parse_attachment_id(object_id)?;

    let url = req.url()?;
    // 鉴权：登录态或针对本邮件的下载票据。
    auth::require_attachment_access(req, &url, ctx.cfg, email_id)?;

    // 先查元数据，确认该附件确实属于这封邮件（防止越权探测任意 R2 key）。
    let db = ctx.db()?;
    let metas = db::get_attachments(&db, email_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let meta = metas
        .iter()
        .find(|a| a.index == index)
        .ok_or(AppError::NotFound)?;
    if !meta.stored {
        return Err(AppError::NotFound);
    }

    let bucket = ctx.bucket()?;
    let key = if meta.key.is_empty() {
        r2::attachment_key(email_id, index)
    } else {
        meta.key.clone()
    };
    let Some(object) = r2::get_object(&bucket, &key).await? else {
        return Err(AppError::NotFound);
    };
    let Some(body) = object.body() else {
        return Err(AppError::NotFound);
    };

    let headers = worker::Headers::new();
    // 只有图片/PDF 才允许 inline，其余一律 attachment，避免 HTML/SVG 型 XSS。
    let inline = meta.inline && r2::inline_allowed(&meta.content_type);
    // 解析出来的 content-type 可能带引号/中文等非法头字符，先清洗成纯 ASCII
    // token，否则 `headers.set` 会失败并变成 500。
    let content_type = if inline {
        sanitize_content_type(&meta.content_type)
    } else {
        "application/octet-stream".to_string()
    };
    headers.set("content-type", &content_type)?;
    headers.set(
        "content-disposition",
        &r2::content_disposition(&meta.filename, inline),
    )?;
    headers.set("x-content-type-options", "nosniff")?;
    // 私有内容：中间层不缓存，浏览器可缓存 10 分钟（token 有效期 600s）。
    headers.set("cache-control", "private, max-age=600")?;
    // 跨域部署时，浏览器默认不会把 Content-Disposition 暴露给 JS，
    // 前端就拿不到文件名。显式放开（仅这两个头，不放开整个响应）。
    headers.set(
        "access-control-expose-headers",
        "content-disposition, content-length",
    )?;

    // CSP 对**所有**附件都设，不只 inline：
    // 即使 Content-Disposition 因为某种原因退化、浏览器选择内联渲染，
    // `default-src 'none'` 也能确保里面的脚本不会执行。
    // inline 的情况额外允许 data: 图片（邮件正文里的内嵌图）。
    headers.set(
        "content-security-policy",
        if inline {
            "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; sandbox"
        } else {
            "default-src 'none'; sandbox"
        },
    )?;

    // 交给运行时直接 pipe，零拷贝转发。
    Ok(Response::from_body(body.response_body()?)?.with_headers(headers))
}

/// 把 MIME 类型清洗成可安全写进响应头的 ASCII 形式。
///
/// 只保留 `type/subtype` 与合法参数，其它字符一律丢弃；实在解析不出来就
/// 退回 `application/octet-stream`（宁可让浏览器下载，也不写出畸形响应头）。
fn sanitize_content_type(raw: &str) -> String {
    let head = raw.split(';').next().unwrap_or("").trim();
    let mut parts = head.splitn(2, '/');
    let ty = parts.next().unwrap_or("");
    let sub = parts.next().unwrap_or("");

    let valid = |s: &str| {
        !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-' | b'_'))
    };

    if valid(ty) && valid(sub) {
        format!("{ty}/{sub}")
    } else {
        "application/octet-stream".to_string()
    }
}

/// 下载原始 `.eml`。
///
/// 与附件一样接受两种凭证，方便前端直接用 `<a download>` 打开。
async fn download_raw(req: &Request, ctx: &ApiCtx<'_>, id: &str) -> AppResult<Response> {
    if !ctx.cfg.store_raw_eml {
        return Err(AppError::NotFound);
    }

    // 下载票据 / Bearer 二选一（浏览器直接打开链接时没有 Authorization 头）。
    let url = req.url()?;
    auth::require_attachment_access(req, &url, ctx.cfg, id)?;

    let bucket = ctx.bucket()?;
    let Some(object) = r2::get_object(&bucket, &r2::raw_key(id)).await? else {
        return Err(AppError::NotFound);
    };
    let Some(body) = object.body() else {
        return Err(AppError::NotFound);
    };

    let headers = worker::Headers::new();
    headers.set("content-type", "message/rfc822")?;
    headers.set(
        "content-disposition",
        &format!("attachment; filename=\"{id}.eml\""),
    )?;
    headers.set("cache-control", "no-store")?;

    Ok(Response::from_body(body.response_body()?)?.with_headers(headers))
}

// ===========================================================================
// 设置
// ===========================================================================

async fn get_settings(ctx: &ApiCtx<'_>) -> AppResult<Response> {
    let db = ctx.db()?;
    let settings = config::load_settings(&db).await?;
    Ok(ok_json(&settings.redacted()))
}

/// 更新设置。
///
/// 语义：字段缺省表示「保持不变」，空字符串表示「清空」。
/// 这样前端不需要把已经脱敏的 token 再回传，避免把 `****abcd` 写成真 token。
#[derive(Deserialize)]
struct SettingsPatch {
    #[serde(default)]
    telegram_enabled: Option<bool>,
    #[serde(default)]
    telegram_bot_token: Option<String>,
    #[serde(default)]
    telegram_chat_id: Option<String>,
    #[serde(default)]
    bark_enabled: Option<bool>,
    #[serde(default)]
    bark_server: Option<String>,
    #[serde(default)]
    bark_device_key: Option<String>,
}

async fn put_settings(req: &mut Request, ctx: &ApiCtx<'_>) -> AppResult<Response> {
    let patch: SettingsPatch = req
        .json()
        .await
        .map_err(|_| AppError::BadRequest("请求体必须是 JSON".into()))?;

    let db = ctx.db()?;
    let mut settings = config::load_settings(&db).await?;

    if let Some(v) = patch.telegram_enabled {
        settings.telegram_enabled = v;
    }
    if let Some(v) = patch.telegram_chat_id {
        settings.telegram_chat_id = v;
    }
    if let Some(v) = patch.bark_enabled {
        settings.bark_enabled = v;
    }
    if let Some(v) = patch.bark_server {
        settings.bark_server = v;
    }

    // 敏感字段：空串表示清空，非空才覆盖；脱敏占位符（含 *）一律忽略。
    if let Some(v) = patch.telegram_bot_token.filter(|v| !v.contains('*')) {
        settings.telegram_bot_token = v;
    }
    if let Some(v) = patch.bark_device_key.filter(|v| !v.contains('*')) {
        settings.bark_device_key = v;
    }

    settings = settings.normalized();

    // 基本校验，避免把明显错误的配置存进去后才发现收不到通知。
    if settings.telegram_enabled
        && (settings.telegram_bot_token.is_empty() || settings.telegram_chat_id.is_empty())
    {
        return Err(AppError::BadRequest(
            "启用 Telegram 推送必须同时填写 Bot Token 与 Chat ID".into(),
        ));
    }
    if settings.bark_enabled && settings.bark_device_key.is_empty() {
        return Err(AppError::BadRequest(
            "启用 Bark 推送必须填写 Device Key".into(),
        ));
    }
    if !settings.bark_server.starts_with("https://") && !settings.bark_server.starts_with("http://")
    {
        return Err(AppError::BadRequest(
            "Bark 服务地址必须以 http:// 或 https:// 开头".into(),
        ));
    }

    config::save_settings(&db, &settings).await?;

    Ok(ok_json(&settings.redacted()))
}

