//! MailFlare —— 极简单用户邮件管理系统。
//!
//! 单个 Worker 同时承担两个角色：
//! 1. `fetch` 事件：`/api/*` 走 Rust 后端，其余请求回落到 `[assets]` 绑定的静态资源
//!    （单页前端）；未配置 assets 时返回内嵌的极简页面。
//! 2. `email` 事件：接收 Cloudflare Email Routing 投递的邮件。
//!
//! 模块划分：
//! * `api`      —— HTTP 路由与各个 handler
//! * `auth`     —— Turnstile 校验、JWT 签发/校验
//! * `config`   —— 环境变量与 `configs` 表
//! * `db`       —— D1 数据访问
//! * `inbound`  —— 收信流水线
//! * `mail`     —— mail-parser 解析
//! * `notify`   —— Telegram / Bark 推送
//! * `r2`       —— R2 对象读写
//! * `util`     —— 时间、ID、截断等小工具

mod api;
mod auth;
mod config;
mod db;
mod error;
mod inbound;
mod jwt;
mod mail;
mod notify;
mod r2;
mod util;

#[cfg(test)]
mod tests;

use worker::*;

/// 未绑定 `ASSETS` 时给出的提示页。
///
/// 刻意不把整个前端 `include_str!` 进来：单页前端有几十 KB，塞进 wasm 会
/// 直接增加冷启动时间；静态资源交给 `[assets]` 托管，边缘缓存更划算。
const NO_ASSETS_HTML: &str = r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><title>MailFlare</title>
<style>body{font:15px/1.7 -apple-system,system-ui,sans-serif;max-width:640px;margin:12vh auto;padding:0 24px;color:#222}
code{background:#f2f4f7;padding:2px 6px;border-radius:5px}pre{background:#f7f8fa;padding:14px;border-radius:10px;overflow:auto}</style>
</head><body>
<h2>MailFlare 已启动 🎉</h2>
<p>后端 API 工作正常，但还没有绑定前端静态资源。请在 <code>wrangler.toml</code> 中添加：</p>
<pre>[assets]
directory = "./public"
binding = "ASSETS"</pre>
<p>然后执行 <code>wrangler deploy</code>。你也可以直接用 <code>curl</code> 查看引导配置：</p>
<pre>curl https://&lt;你的域名&gt;/api/auth/config</pre>
</body></html>"#;

/// HTTP 入口。
#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();

    // ---- API 路由 -------------------------------------------------------
    if path == "/api" || path.starts_with("/api/") {
        return Ok(api::dispatch(req, env, &path).await);
    }

    // ---- 静态资源 -------------------------------------------------------
    // `[assets]` 绑定由 Cloudflare 的边缘网络直接托管，自带 ETag / Range / Brotli。
    // 这里把请求原样转发给它，而不是自己读文件——静态资源永远不会进入 WASM 堆。
    if let Ok(assets) = env.assets("ASSETS") {
        // 直接把原始请求转发给资产层：ETag、Range、压缩、SPA 回落都由它处理。
        match assets.fetch_request(req).await {
            Ok(resp) => return Ok(resp),
            Err(e) => console_error!("[mailflare] 静态资源处理失败: {e}"),
        }
        // req 已被 move，这里只能返回错误。
        return Response::error("静态资源不可用", 500);
    }

    // 兜底：没有 ASSETS 绑定时，给一个自检页而不是干巴巴的 404。
    let last_segment = path.rsplit('/').next().unwrap_or_default();
    if path == "/" || !last_segment.contains('.') {
        let headers = Headers::new();
        headers.set("content-type", "text/html; charset=utf-8")?;
        headers.set("cache-control", "no-cache")?;
        return Ok(Response::ok(NO_ASSETS_HTML)?.with_headers(headers));
    }

    Response::error("Not Found", 404)
}

/// Email 事件入口。
///
/// 用 `#[event(email)]` 而不是手写 `wasm_bindgen` 导出：worker-macros 会生成
/// 正确的 `EmailExports.email()`，并把 `worker_sys::Context` 包装成 `Context`。
#[event(email)]
async fn email_handler(message: ForwardableEmailMessage, env: Env, ctx: Context) -> Result<()> {
    inbound::handle(message, env, ctx).await
}
