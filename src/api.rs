//! HTTP API 层。
//!
//! 路由表（全部挂在 `/api` 下，命中 `/api/*` 之外的请求回落到静态资源）：
//! | 方法   | 路径                     | 说明                              |
//! |--------|--------------------------|-----------------------------------|
//! | POST   | /api/auth/login          | 用户名 + 密码 + Turnstile 换 JWT  |
//! | POST   | /api/auth/logout         | 纯前端丢弃 token（占位）          |
//! | GET    | /api/auth/session        | 校验当前 token 是否仍有效         |
//! | GET    | /api/emails              | 分页列表（支持未读筛选/游标）     |
//! | GET    | /api/emails/:id          | 详情（自动标记已读）              |
//! | DELETE | /api/emails/:id          | 删除 D1 记录 + R2 附件            |
//! | POST   | /api/emails/:id/read     | 标记已读/未读                     |
//! | GET    | /api/attachments/:id     | 流式下载 R2 附件                  |
//! | GET    | /api/settings            | 读取推送配置（脱敏）              |
//! | PUT    | /api/settings            | 更新推送配置                      |

use serde::{Deserialize, Serialize};
use worker::{console_error, D1Database, Env, Request, Response};

use crate::auth::{self, SCOPE_DL};
use crate::config::{self, Config, Settings};
use crate::db::{self, ListQuery};
use crate::error::{AppError, AppResult};
use crate::r2;
use crate::util::{ok_json, query_i64, query_param};

const DL_TICKET_TTL: i64 = 600;

pub struct ApiCtx<'a> {
    pub env: &'a Env,
    /// 环境变量里的登录凭证。
    pub cfg: &'a Config,
    /// D1 里的后台设置。
    pub settings: &'a Settings,
}

impl<'a> ApiCtx<'a> {
    fn db(&self) -> AppResult<D1Database> {
        Ok(self.env.d1("DB")?)
    }
}

/// 前端与 API 同源部署，不需要 CORS。
pub async fn dispatch(mut req: Request, env: Env, path: &str) -> Response {
    let cfg = match Config::from_env(&env) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    // 读不到设置时用默认值，保证「没配 D1」也能登录进来看错误。
    let settings = match env.d1("DB") {
        Ok(db) => config::load_settings(&db).await.unwrap_or_else(|e| {
            console_error!("[mailflare] 读取设置失败，使用默认值: {e:?}");
            Settings::default()
        }),
        Err(e) => {
            console_error!("[mailflare] 无法访问 D1: {e:?}");
            Settings::default()
        }
    };
    let ctx = ApiCtx {
        env: &env,
        cfg: &cfg,
        settings: &settings,
    };
    match route(&mut req, &ctx, path).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
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
        // 前端引导：告诉登录页要不要渲染 Turnstile 挂件、用哪个 site key。
        // 它必须在登录前可访问，所以放在这里，且只读 D1 设置。
        "/api/auth/config" => {
            require_method(&method, worker::Method::Get)?;
            return Ok(ok_json(&AuthConfigResponse {
                turnstile_site_key: ctx.settings.turnstile_site_key.clone(),
                turnstile_required: ctx.settings.turnstile_enabled(),
                site_name: ctx.settings.site_name.clone(),
                site_logo: ctx.settings.site_logo.clone(),
                site_background: public_background(&ctx.settings.site_background),
            }));
        }
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
                expires_in: auth::TOKEN_TTL_SECS,
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
        "/api/settings/test-push" => {
            require_method(&method, worker::Method::Post)?;
            test_push(ctx).await
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
    /// 管理员用户名。
    username: String,
    password: String,
    /// Turnstile token；后台未配 secret 时可为空。
    #[serde(default)]
    turnstile_token: String,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
    expires_in: i64,
}

/// 登录页需要的公开配置。
///
/// 登录前就要拿到站点外观，所以这里也带上 `site_*`。注意**不能**放任何
/// 密钥字段——这个接口无需鉴权。
#[derive(Serialize)]
struct AuthConfigResponse {
    turnstile_site_key: String,
    turnstile_required: bool,
    site_name: String,
    site_logo: String,
    site_background: String,
}

#[derive(Serialize)]
struct SessionResponse {
    authenticated: bool,
    expires_in: i64,
}

/// 登录：Turnstile -> 用户名 + 密码 -> 签发 JWT。
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
    if let Err(e) =
        auth::verify_turnstile(ctx.settings, &body.turnstile_token, client_ip.as_deref()).await
    {
        // Turnstile 失败也计入失败次数，防止刷接口。
        record_failure(&db, &mut guard, now).await?;
        return Err(e);
    }

    // ---- 2. 用户名 + 密码 ----------------------------------------------
    if !auth::verify_credentials(ctx.cfg, &body.username, &body.password) {
        // 不区分「用户名不存在」与「密码错误」：避免泄露用户名是否正确。
        record_failure(&db, &mut guard, now).await?;
        return Err(AppError::Forbidden("用户名或密码错误".into()));
    }

    // ---- 3. 成功：清空计数并签发 token ---------------------------------
    guard = config::LoginGuard::default();
    guard.save(&db).await?;

    let token = auth::issue_token(
        ctx.cfg,
        auth::SCOPE_API,
        &ctx.cfg.admin_username,
        auth::TOKEN_TTL_SECS,
    )?;
    Ok(ok_json(&LoginResponse {
        token,
        expires_in: auth::TOKEN_TTL_SECS,
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
        ctx.settings.page_size_default,
        1,
        ctx.settings.page_size_max,
    );

    let cursor = query_param(&url, "cursor").and_then(|c| parse_cursor(&c));

    // 关键词带长度上限：LIKE 全表扫描，过长的串纯属浪费。
    let search = query_param(&url, "q")
        .map(|q| q.trim().chars().take(120).collect::<String>())
        .filter(|q| !q.is_empty());

    let db = ctx.db()?;
    let (items, next_cursor, unread) = db::list_emails(
        &db,
        &ListQuery {
            unread_only,
            cursor,
            limit,
            search,
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
    /// 从这封邮件里提取到的验证码；没有则为空。
    otp_code: Option<String>,
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

    // 验证码在收信推送时已提取过一次，这里对详情实时再提取一次透出给前端，
    // 用户不用进正文里手动找码。
    let otp_code = {
        let source = if email.text_body.is_empty() {
            &email.html_body
        } else {
            &email.text_body
        };
        crate::otp::extract(&email.subject, source).map(|c| c.value)
    };

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

    Ok(ok_json(&EmailDetailResponse {
        email,
        attachment_urls,
        otp_code,
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
    r2::delete_email_objects(ctx.settings, &attachments).await;

    Ok(ok_json(&serde_json::json!({ "id": id, "deleted": true })))
}

// ===========================================================================
// 附件下载 / 原始邮件下载
// ===========================================================================

/// 流式读取 R2 附件（S3 API）。
///
/// 零拷贝：不调用 `bytes().await`，直接用 `ResponseBody::Stream` 转发。
/// 上游响应的 headers 在运行时里不可变，所以必须 `into_parts()` 取出 body
/// 后重建响应。
async fn get_attachment(req: &Request, ctx: &ApiCtx<'_>, object_id: &str) -> AppResult<Response> {
    let (email_id, index) = r2::parse_attachment_id(object_id)?;

    let url = req.url()?;
    // 1. 前置鉴权：登录态或针对本邮件的下载票据。
    //    必须在任何上游请求**之前**完成，避免成为探测工具。
    auth::require_attachment_access(req, &url, ctx.cfg, email_id)?;

    // 2. 查元数据，确认该附件确实属于这封邮件（防止越权探测任意 R2 key）。
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

    let key = if meta.key.is_empty() {
        r2::attachment_key(email_id, index)
    } else {
        meta.key.clone()
    };

    // 3. 把客户端的 Range 头透传给 S3（不参与签名）。
    let range = req.headers().get("range").ok().flatten();
    let Some(upstream) =
        r2::get_attachment(ctx.settings, &key, range.as_deref()).await?
    else {
        return Err(AppError::NotFound);
    };
    let status = upstream.status_code();

    // 4. 先把需要的上游响应头复制成独立 String，再取 body。
    //    顺序很重要：`headers()` 借用了 `upstream`，而 `into_parts()` 会把它
    //    移走，所以必须先把值拷出来（这几个头都很短，拷贝成本可忽略）。
    let (content_range, upstream_length, etag, accept_ranges) = {
        let h = upstream.headers();
        (
            h.get("content-range").ok().flatten(),
            h.get("content-length").ok().flatten(),
            h.get("etag").ok().flatten(),
            h.get("accept-ranges").ok().flatten(),
        )
    };

    // `into_parts()` 直接搬走 ResponseBody（Stream 变体是 ReadableStream），
    // 不经过任何字节拷贝。
    let (_, body) = upstream.into_parts();

    // 5. 重建响应：状态码透传（200 / 206），头部我们说了算。
    let headers = worker::Headers::new();

    // 只有图片/PDF 才允许 inline，其余一律 attachment，避免 HTML/SVG 型 XSS。
    let inline = meta.inline && r2::inline_allowed(&meta.content_type);
    // 优先用 D1 里记录的 MIME（我们入库时已经清洗过）；缺失时退回上游的。
    let content_type = if inline {
        r2::sanitize_content_type(&meta.content_type)
    } else {
        "application/octet-stream".to_string()
    };
    headers.set("content-type", &content_type)?;
    headers.set(
        "content-disposition",
        &r2::content_disposition(&meta.filename, inline),
    )?;

    // 三大安全头：对所有附件都设，不只 inline。
    headers.set("x-content-type-options", "nosniff")?;
    // 即使 Content-Disposition 退化成内联渲染，`default-src 'none'`
    // 也能确保里面的脚本不会执行。inline 时额外允许 data: 图片
    // （邮件正文里的内嵌图）。
    headers.set(
        "content-security-policy",
        if inline {
            "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; sandbox"
        } else {
            "default-src 'none'; sandbox"
        },
    )?;

    // Range 续传相关头部：206 时把 Content-Range 透传回去，
    // 否则浏览器无法知道拿到的是哪一段，断点续传会坏。
    headers.set(
        "accept-ranges",
        accept_ranges.as_deref().unwrap_or("bytes"),
    )?;
    if let Some(value) = content_range {
        headers.set("content-range", &value)?;
    }
    // 长度必须透传：206 时上游给的是分片长度，不是对象总长。
    if let Some(value) = upstream_length {
        headers.set("content-length", &value)?;
    }
    // 上游若给了 ETag，透传出去让浏览器能走条件请求。
    if let Some(etag) = etag {
        headers.set("etag", &etag)?;
    }

    // 私有内容：中间层不缓存，浏览器可缓存 10 分钟（下载票据有效期 600s）。
    headers.set("cache-control", "private, max-age=600")?;
    // 浏览器默认不会把 Content-Disposition 暴露给 JS，前端就拿不到文件名。
    // 只放开这几个头，不放开整个响应。
    headers.set(
        "access-control-expose-headers",
        "content-disposition, content-length, content-range, accept-ranges",
    )?;

    // 6. 用新的头部 + 上游的流式 body 组装响应。
    //    `Response::from_body` 直接接管 `ResponseBody`（Stream 变体是
    //    ReadableStream），全程零拷贝。`with_status` 透传 200 / 206，
    //    206 时浏览器才能正确拼接续传片段。
    Ok(Response::from_body(body)?
        .with_headers(headers)
        .with_status(status))
}

// ===========================================================================
// 设置
// ===========================================================================

/// 单个渠道的测试结果，直接给前端展示。
#[derive(Serialize)]
struct TestPushChannel {
    name: &'static str,
    /// `ok` / `skipped` / `failed`
    status: &'static str,
    message: String,
}

fn describe(name: &'static str, result: crate::notify::ChannelResult) -> TestPushChannel {
    use crate::notify::ChannelResult;
    match result {
        ChannelResult::Ok => TestPushChannel {
            name,
            status: "ok",
            message: "已发送，请查看客户端".into(),
        },
        ChannelResult::Skipped => TestPushChannel {
            name,
            status: "skipped",
            message: "未启用，已跳过".into(),
        },
        ChannelResult::Failed(message) => TestPushChannel {
            name,
            status: "failed",
            message,
        },
    }
}

/// 发送测试推送。
///
/// 用 `ctx.settings`（dispatch 已读过库），所以测的是**保存后**的配置。
/// 前端若改了表单还没保存，这里测不到新值——按钮旁有说明。
async fn test_push(ctx: &ApiCtx<'_>) -> AppResult<Response> {
    let (telegram, bark) = crate::notify::test_push(ctx.settings).await;
    let channels = [
        describe("Telegram", telegram),
        describe("Bark", bark),
    ];
    // 两个都没启用时提示用户，而不是返回一个「成功但什么都没发」的假结果。
    let all_skipped = channels.iter().all(|c| c.status == "skipped");
    Ok(ok_json(&serde_json::json!({
        "channels": channels,
        "all_skipped": all_skipped,
    })))
}

async fn get_settings(ctx: &ApiCtx<'_>) -> AppResult<Response> {
    // dispatch() 已经把设置读进 ctx.settings，这里直接用，不再查一次库。
    Ok(ok_json(&ctx.settings.redacted()))
}

/// 图片地址是否可安全地写进 CSS / `<img src>`。
///
/// 只放行 https、站内相对路径（单个 `/`，不能是 `//host`）和 `data:image`。
/// 拒绝 `http:`（混合内容）与 `javascript:` 这类伪协议——它们会被直接塞进
/// `style` 或 `src`，等于开了存储型 XSS 的口子。
pub(crate) fn safe_image_source(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.len() > 2048 {
        return false;
    }
    // 控制字符常被用来绕过前缀检查。
    if value.chars().any(|c| c.is_control()) {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("data:image/") || lower.starts_with("https://") {
        return true;
    }
    // `//host` 是协议相对 URL，会指向外部，必须排除。
    value.starts_with('/') && !value.starts_with("//")
}

/// 把背景配置转成可直接用的 CSS 值。
///
/// 只接受 https 图片地址；输出写进内联 `style`，所以必须先过 [`safe_image_source`]。
/// 空串表示使用默认背景。
pub(crate) fn public_background(raw: &str) -> String {
    let value = raw.trim();
    if value.is_empty() || !value.to_ascii_lowercase().starts_with("https://") {
        return String::new();
    }
    if !safe_image_source(value) {
        return String::new();
    }
    // 地址里可能有引号，转义后再包，避免提前闭合 url()。
    format!("url(\"{}\")", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 设置的可写字段。
///
/// 全部 `Option`：`None` 表示「保持不变」，`Some("")` 表示「清空」。
/// 这样前端不需要把脱敏后的 `****abcd` 再回传，避免把占位符写成真密钥。
#[derive(Deserialize, Default)]
#[serde(default)]
struct SettingsPatch {
    site_name: Option<String>,
    site_logo: Option<String>,
    site_background: Option<String>,

    turnstile_site_key: Option<String>,
    turnstile_secret: Option<String>,

    telegram_enabled: Option<bool>,
    telegram_bot_token: Option<String>,
    telegram_chat_id: Option<String>,

    bark_enabled: Option<bool>,
    bark_server: Option<String>,
    bark_device_key: Option<String>,

    r2_account_id: Option<String>,
    r2_access_key_id: Option<String>,
    r2_secret_access_key: Option<String>,
    r2_bucket: Option<String>,

    blocked_senders: Option<Vec<String>>,
    allowed_recipients: Option<Vec<String>>,
    max_body_bytes: Option<i64>,
    max_message_bytes: Option<i64>,
    max_attachment_bytes: Option<i64>,
    r2_concurrency: Option<i64>,

    page_size_default: Option<i64>,
    page_size_max: Option<i64>,
}

/// 把 `Settings` 上某个字段覆盖为 patch 里的值。
///
/// 用一个宏而不是逐字段写 `if let Some(v) = patch.x { s.x = v; }`：
/// 20 多个字段手写既啰嗦又容易漏。
macro_rules! apply {
    ($patch:expr => $settings:expr; $($field:ident),* $(,)?) => {
        $(
            if let Some(v) = $patch.$field {
                $settings.$field = v;
            }
        )*
    };
}

async fn put_settings(req: &mut Request, ctx: &ApiCtx<'_>) -> AppResult<Response> {
    let patch: SettingsPatch = req
        .json()
        .await
        .map_err(|_| AppError::BadRequest("请求体必须是 JSON".into()))?;

    let db = ctx.db()?;
    let mut settings = config::load_settings(&db).await?;

    // 布尔与数值：有值就覆盖。
    apply!(patch => settings;
        telegram_enabled, bark_enabled,
        max_body_bytes, max_message_bytes, max_attachment_bytes, r2_concurrency,
        page_size_default, page_size_max,
    );

    // 非密钥字符串：有值就覆盖（空串即清空）。
    apply!(patch => settings;
        site_name, site_logo, site_background,
        turnstile_site_key,
        telegram_chat_id, bark_server,
        r2_account_id, r2_bucket,
    );

    // 密钥字段：仅当值不含 `*` 时才覆盖，防止把脱敏占位符写回去。
    // 空串是有意义的（表示清空），所以要保留。
    config::merge_secret(&mut settings.turnstile_secret, patch.turnstile_secret);
    config::merge_secret(&mut settings.telegram_bot_token, patch.telegram_bot_token);
    config::merge_secret(&mut settings.bark_device_key, patch.bark_device_key);
    config::merge_secret(&mut settings.r2_access_key_id, patch.r2_access_key_id);
    config::merge_secret(&mut settings.r2_secret_access_key, patch.r2_secret_access_key);

    // 地址列表：整表替换（前端提交完整数组，不支持逐项增删——
    // 列表项之间没有稳定标识，做增量合并反而容易错）。
    if let Some(list) = patch.blocked_senders {
        settings.blocked_senders = list;
    }
    if let Some(list) = patch.allowed_recipients {
        settings.allowed_recipients = list;
    }

    settings = settings.normalized();

    // ---- 校验：把明显错误的配置挡在写库之前 -----------------------------
    if settings.site_name.chars().count() > 40 {
        return Err(AppError::BadRequest("站点名不能超过 40 个字符".into()));
    }
    // Logo 与背景现在都只接受 https 图片地址。
    for (label, value) in [("Logo", &settings.site_logo), ("背景", &settings.site_background)] {
        if !value.is_empty() && !value.to_ascii_lowercase().starts_with("https://") {
            return Err(AppError::BadRequest(format!(
                "{label}地址必须以 https:// 开头"
            )));
        }
        if !value.is_empty() && !safe_image_source(value) {
            return Err(AppError::BadRequest(format!("{label}地址格式不合法")));
        }
    }
    if !settings.bark_server.starts_with("https://") && !settings.bark_server.starts_with("http://")
    {
        return Err(AppError::BadRequest(
            "Bark 服务地址必须以 http:// 或 https:// 开头".into(),
        ));
    }
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
    if let Some(bad) = settings
        .blocked_senders
        .iter()
        .chain(settings.allowed_recipients.iter())
        .find(|rule| !config::valid_address_rule(rule))
    {
        return Err(AppError::BadRequest(format!(
            "地址规则「{bad}」格式不正确：请填 a@b.com、@b.com 或 b.com"
        )));
    }
    // R2 四项要么全空（不用附件功能），要么全填；填一半最难排查。
    let r2_filled = [
        !settings.r2_account_id.is_empty(),
        !settings.r2_access_key_id.is_empty(),
        !settings.r2_secret_access_key.is_empty(),
        !settings.r2_bucket.is_empty(),
    ];
    let count = r2_filled.iter().filter(|v| **v).count();
    if count != 0 && count != 4 {
        return Err(AppError::BadRequest(
            "R2 的四项（账号 ID / Access Key / Secret Key / 桶名）必须同时填写或同时留空".into(),
        ));
    }
    // 账号 ID 是 32 位十六进制；填成邮箱或 URL 会导致请求 404 且难排查。
    if !settings.r2_account_id.is_empty()
        && !(settings.r2_account_id.len() == 32
            && settings.r2_account_id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(AppError::BadRequest(
            "R2 账号 ID 应是 32 位十六进制字符串（可在控制台右侧栏或 R2 页面找到）".into(),
        ));
    }
    if settings.turnstile_enabled() && settings.turnstile_site_key.is_empty() {
        return Err(AppError::BadRequest(
            "填写了 Turnstile Secret 就必须同时填写 Site Key，否则前端渲染不出验证框".into(),
        ));
    }

    config::save_settings(&db, &settings).await?;
    Ok(ok_json(&settings.redacted()))
}

