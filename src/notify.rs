//! 新邮件推送：Telegram Bot 与 Bark。
//!
//! 设计要点：
//! * 推送在 `ctx.wait_until()` 里执行，不阻塞收信主流程；
//! * 两个渠道用 `futures_util::join!` **并发**发起，总耗时取两者最大值；
//! * 每个请求都带超时，避免某个渠道挂死拖垮 Worker 的墙钟预算；
//! * 推送失败只写日志，绝不影响邮件是否已入库。

use std::time::Duration;

use serde::Serialize;
use worker::{console_error, console_log, Fetch, Method, Request, RequestInit, Url};

use crate::config::Settings;
use crate::util::truncate_utf8;

/// 单次推送的网络超时。
const PUSH_TIMEOUT: Duration = Duration::from_secs(8);
/// Telegram 单条消息上限 4096 字符，这里留出转义/链接的余量。
const TG_TEXT_LIMIT: usize = 3500;

/// 推送所需的邮件摘要。
///
/// 刻意不复用 `EmailDetail`：正文可能有几百 KB，而通知只需要一小段预览，
/// 单独构造能显著降低 waitUntil 任务的内存占用。
#[derive(Debug, Clone)]
pub struct PushEmail {
    pub id: String,
    pub sender: String,
    pub subject: String,
    /// 已裁剪好的正文预览（纯文本）。
    pub preview: String,
    pub attachment_count: usize,
}

/// 推送所需的上下文。
pub struct PushContext<'a> {
    pub settings: &'a Settings,
    pub email: &'a PushEmail,
    /// 管理页地址，用于消息里附上「查看详情」链接（可为空）。
    pub public_url: Option<&'a str>,
}

#[derive(Debug)]
pub enum ChannelResult {
    /// 未启用
    Skipped,
    Ok,
    Failed(String),
}

/// 把错误信息里的换行压平，避免日志串行。
fn flatten(e: impl std::fmt::Display) -> String {
    e.to_string().replace('\n', " ")
}

/// 带超时的 `fetch`。
///
/// Cloudflare 的 `fetch` 没有超时参数，这里用 `AbortController` + `Delay` 竞速：
/// 谁先完成就用谁的结果；超时分支主动 abort，释放底层连接。
async fn fetch_with_timeout(url: Url, init: RequestInit, timeout: Duration) -> Result<u16, String> {
    let controller = worker::AbortController::default();
    // 必须把 signal 真正挂到请求上，否则 abort() 只是空操作。
    // `worker::RequestInit` 没有 signal 字段，所以走 `send_with_signal`。
    let signal = controller.signal();
    let request = Fetch::Request(Request::new_with_init(url.as_str(), &init).map_err(flatten)?);

    let send = async move {
        let mut resp = request.send_with_signal(&signal).await.map_err(flatten)?;
        // 必须把 body 读掉，连接才会及时归还到连接池。
        let _ = resp.bytes().await;
        Ok::<u16, String>(resp.status_code())
    };

    let timeout_fut = async {
        worker::Delay::from(timeout).await;
        Err::<u16, String>("请求超时".to_string())
    };

    // 用 `select`（函数式）而不是 `select!` 宏：宏要求 future 实现
    // `FusedFuture + Unpin`，而 async 块两者都不满足。
    let send = Box::pin(send);
    let timeout_fut = Box::pin(timeout_fut);

    match futures_util::future::select(send, timeout_fut).await {
        // 正常返回：`_timeout` 是被丢弃的超时 future，`select` 已经把它
        // 从等待集合里摘掉，`Delay` 的 Drop 会清理 setTimeout。
        futures_util::future::Either::Left((result, _timeout)) => result,
        // 超时：`pending_send` 是被放弃的请求 future。把它丢掉之后再 abort，
        // 保证不会出现「已 abort 的请求又被 poll 出结果」的竞态。
        futures_util::future::Either::Right((result, pending_send)) => {
            drop(pending_send);
            controller.abort();
            result
        }
    }
}

/// Telegram MarkdownV2 转义。
///
/// MarkdownV2 要求对一批保留字符加反斜杠，否则整条消息会被 400 拒收。
fn escape_markdown_v2(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for c in input.chars() {
        if matches!(
            c,
            '_' | '*' | '[' | ']' | '(' | ')' | '~' | '`' | '>' | '#' | '+' | '-' | '=' | '|'
                | '{' | '}' | '.' | '!'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// 从 `"张三 <a@b.com>"` 里取显示名。
fn short_sender(sender: &str) -> &str {
    match sender.find('<') {
        Some(idx) => sender[..idx].trim(),
        None => sender.trim(),
    }
}

/// 详情页链接。
fn detail_link(public_url: Option<&str>, id: &str) -> Option<String> {
    public_url.map(|base| format!("{}/?id={}", base.trim_end_matches('/'), id))
}

/// 构造 Telegram 消息文本（已转义）。
fn telegram_text(ctx: &PushContext<'_>) -> String {
    let d = ctx.email;
    let mut text = String::with_capacity(640);

    text.push_str("*📬 新邮件*\n\n");
    let name = short_sender(&d.sender);
    text.push_str(&format!("*发件人*：{}\n", escape_markdown_v2(name)));
    if name != d.sender && !name.is_empty() {
        text.push_str(&format!("*地址*：{}\n", escape_markdown_v2(&d.sender)));
    }
    text.push_str(&format!("*主题*：{}\n", escape_markdown_v2(&d.subject)));

    if d.attachment_count > 0 {
        text.push_str(&format!("*附件*：{} 个\n", d.attachment_count));
    }

    let preview = d.preview.trim();
    if !preview.is_empty() {
        // 先按字符数截断（中文场景更贴近视觉长度），再按字节兜底。
        let snippet: String = preview.chars().take(600).collect();
        text.push('\n');
        text.push_str(&escape_markdown_v2(snippet.trim()));
        text.push('\n');
    }

    if let Some(link) = detail_link(ctx.public_url, &d.id) {
        text.push_str(&format!(
            "\n[在 MailFlare 中查看]({})",
            escape_markdown_v2(&link)
        ));
    }

    // 硬性截断，保证不超过 Telegram 的 4096 上限。
    truncate_utf8(text, TG_TEXT_LIMIT)
}

#[derive(Serialize)]
struct TelegramPayload<'a> {
    chat_id: &'a str,
    text: String,
    parse_mode: &'static str,
    disable_web_page_preview: bool,
}

async fn push_telegram(ctx: &PushContext<'_>) -> ChannelResult {
    let s = ctx.settings;
    if !s.telegram_enabled {
        return ChannelResult::Skipped;
    }
    if s.telegram_bot_token.is_empty() || s.telegram_chat_id.is_empty() {
        return ChannelResult::Failed("Telegram token 或 chat_id 未配置".into());
    }

    let payload = TelegramPayload {
        chat_id: &s.telegram_chat_id,
        text: telegram_text(ctx),
        parse_mode: "MarkdownV2",
        disable_web_page_preview: true,
    };

    // token 只含 [0-9A-Za-z:_-]，无需 URL 编码。
    let Ok(url) = Url::parse(&format!(
        "https://api.telegram.org/bot{}/sendMessage",
        s.telegram_bot_token
    )) else {
        return ChannelResult::Failed("Telegram bot token 含非法字符".into());
    };

    let Ok(body) = serde_json::to_string(&payload) else {
        return ChannelResult::Failed("Telegram 请求体序列化失败".into());
    };

    let headers = worker::Headers::new();
    if headers.set("content-type", "application/json").is_err() {
        return ChannelResult::Failed("构造请求头失败".into());
    }

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body)));

    match fetch_with_timeout(url, init, PUSH_TIMEOUT).await {
        Ok(200..=299) => ChannelResult::Ok,
        Ok(code) => ChannelResult::Failed(format!("Telegram 返回 HTTP {code}")),
        Err(e) => ChannelResult::Failed(e),
    }
}

/// Bark v2 推送请求体。
#[derive(Serialize)]
struct BarkPayload<'a> {
    device_key: &'a str,
    title: String,
    body: String,
    group: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

async fn push_bark(ctx: &PushContext<'_>) -> ChannelResult {
    let s = ctx.settings;
    if !s.bark_enabled {
        return ChannelResult::Skipped;
    }
    if s.bark_device_key.is_empty() {
        return ChannelResult::Failed("Bark device key 未配置".into());
    }

    let preview: String = ctx.email.preview.trim().chars().take(400).collect();
    let payload = BarkPayload {
        device_key: &s.bark_device_key,
        title: format!("📬 {}", ctx.email.subject),
        body: format!("{}\n{}", short_sender(&ctx.email.sender), preview),
        group: "MailFlare",
        url: detail_link(ctx.public_url, &ctx.email.id),
    };

    // 统一走 POST {server}/push：对长文本与特殊字符最安全。
    let Ok(url) = Url::parse(&format!("{}/push", s.bark_server)) else {
        return ChannelResult::Failed("Bark 服务地址非法".into());
    };

    let Ok(body) = serde_json::to_string(&payload) else {
        return ChannelResult::Failed("Bark 请求体序列化失败".into());
    };

    let headers = worker::Headers::new();
    if headers.set("content-type", "application/json").is_err() {
        return ChannelResult::Failed("构造请求头失败".into());
    }

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body)));

    match fetch_with_timeout(url, init, PUSH_TIMEOUT).await {
        Ok(200..=299) => ChannelResult::Ok,
        Ok(code) => ChannelResult::Failed(format!("Bark 返回 HTTP {code}")),
        Err(e) => ChannelResult::Failed(e),
    }
}

/// 并发推送两个渠道。
pub async fn dispatch(ctx: &PushContext<'_>) {
    let (telegram, bark) = futures_util::join!(push_telegram(ctx), push_bark(ctx));

    // 日志带上邮件 id，便于区分「没推」和「推失败」。
    for (name, r) in [("telegram", &telegram), ("bark", &bark)] {
        match r {
            ChannelResult::Failed(msg) => {
                console_error!("[mailflare] {name} 推送失败 ({}): {msg}", ctx.email.id)
            }
            ChannelResult::Ok => console_log!("[mailflare] {name} 推送成功 ({})", ctx.email.id),
            ChannelResult::Skipped => {}
        }
    }
}
