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

/// Telegram MarkdownV2 中 **链接目标（`(...)` 内部）** 的转义。
///
/// 官方规则与正文不同：URL 里只需要转义 `\` 和 `)`，其余字符一律原样保留。
/// 用正文那套「全量转义」处理 URL 会得到
/// `https://mail\.example\.com/?id\=...` —— 反斜杠会被当成 URL 的一部分，
/// 用户点开就是 404。这是最容易踩的坑之一。
fn escape_markdown_v2_url(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 4);
    for c in input.chars() {
        if matches!(c, '\\' | ')') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Telegram MarkdownV2 正文转义。
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
            escape_markdown_v2_url(&link)
        ));
    }

    // 硬性截断，保证不超过 Telegram 的 4096 上限。
    truncate_utf8(text, TG_TEXT_LIMIT)
}

#[derive(Serialize)]
struct TelegramPayload<'a> {
    chat_id: &'a str,
    text: String,
    /// `None` 表示纯文本（不带任何 Markdown 解析）。
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<&'static str>,
    disable_web_page_preview: bool,
}

/// 发送一条 Telegram 文本消息。
///
/// 抽出这一层是为了让「新邮件通知」和「收信失败告警」共用同一套
/// URL 构造、序列化、超时与错误处理，避免两份容易走样的副本。
/// 发送一条 Telegram 消息，失败时自动降级为纯文本重试。
///
/// ## 为什么需要降级
/// MarkdownV2 对转义极其苛刻：只要漏掉一个保留字符（邮件标题里出现
/// 一个未配对的 `*`、发件人名里的 `_`、URL 里的 `)`……），Telegram 就直接
/// 返回 `400 Bad Request: can't parse entities`，**整条通知丢失**。
/// 手工转义很难覆盖所有情况，所以这里做两层保险：
/// 1. 正文与链接分别按官方规则转义（见 `escape_markdown_v2*`）。
/// 2. 仍然失败（400）时，去掉 `parse_mode` 用**纯文本**重发一次——
///    宁可丢掉粗体，也不能丢掉通知本身。
async fn send_telegram_text(settings: &Settings, text: String) -> ChannelResult {
    if settings.telegram_bot_token.is_empty() || settings.telegram_chat_id.is_empty() {
        return ChannelResult::Failed("Telegram token 或 chat_id 未配置".into());
    }

    // 第一次：MarkdownV2（带格式）
    match post_telegram(settings, &text, Some("MarkdownV2")).await {
        Ok(200..=299) => ChannelResult::Ok,
        // 400 基本都来自解析失败 —— 换纯文本再试。
        Ok(400) => {
            console_log!("[mailflare] Telegram MarkdownV2 解析失败，降级为纯文本重试");
            // 纯文本模式下要抹掉转义用的反斜杠，否则用户会看到 `\\*` 这种东西。
            let plain = strip_markdown_escapes(&text);
            match post_telegram(settings, &plain, None).await {
                Ok(200..=299) => ChannelResult::Ok,
                Ok(code) => ChannelResult::Failed(format!("Telegram 返回 HTTP {code}（纯文本降级也失败）")),
                Err(e) => ChannelResult::Failed(e),
            }
        }
        Ok(code) => ChannelResult::Failed(format!("Telegram 返回 HTTP {code}")),
        Err(e) => ChannelResult::Failed(e),
    }
}

/// 实际发出 `sendMessage` 请求。
///
/// 用 **POST + JSON body**（不是 query string）：标题里的 `/`、`?`、`#`、
/// `&`、`%` 等字符放进 URL 都需要额外编码，稍有不慎就会被 Telegram 或
/// 中间层截断；JSON body 完全没有这个问题。
async fn post_telegram(
    settings: &Settings,
    text: &str,
    parse_mode: Option<&'static str>,
) -> Result<u16, String> {
    let payload = TelegramPayload {
        chat_id: &settings.telegram_chat_id,
        text: text.to_string(),
        parse_mode,
        disable_web_page_preview: true,
    };

    // token 只含 [0-9A-Za-z:_-]，无需 URL 编码。
    let url = Url::parse(&format!(
        "https://api.telegram.org/bot{}/sendMessage",
        settings.telegram_bot_token
    ))
    .map_err(|_| "Telegram bot token 含非法字符".to_string())?;

    let body =
        serde_json::to_string(&payload).map_err(|e| format!("Telegram 请求体序列化失败: {e}"))?;

    let headers = worker::Headers::new();
    if headers.set("content-type", "application/json").is_err() {
        return Err("构造请求头失败".into());
    }

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body)));

    fetch_with_timeout(url, init, PUSH_TIMEOUT).await
}

/// 去掉 MarkdownV2 转义用的反斜杠（降级为纯文本时使用）。
fn strip_markdown_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // 转义符后面的字符原样输出；末尾孤立的反斜杠直接丢弃。
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
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

/// 发送一条 Bark 消息。同样抽成原语供告警复用。
async fn send_bark_text(
    settings: &Settings,
    title: String,
    body: String,
    url: Option<String>,
) -> ChannelResult {
    if settings.bark_device_key.is_empty() {
        return ChannelResult::Failed("Bark device key 未配置".into());
    }

    let payload = BarkPayload {
        device_key: &settings.bark_device_key,
        title,
        body,
        group: "MailFlare",
        url,
    };

    // 统一走 POST {server}/push：对长文本与特殊字符最安全。
    let Ok(url) = Url::parse(&format!("{}/push", settings.bark_server)) else {
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

/// 新邮件的 Telegram 通知。
async fn push_telegram(ctx: &PushContext<'_>) -> ChannelResult {
    if !ctx.settings.telegram_enabled {
        return ChannelResult::Skipped;
    }
    send_telegram_text(ctx.settings, telegram_text(ctx)).await
}

/// 新邮件的 Bark 通知。
async fn push_bark(ctx: &PushContext<'_>) -> ChannelResult {
    let s = ctx.settings;
    if !s.bark_enabled {
        return ChannelResult::Skipped;
    }

    let preview: String = ctx.email.preview.trim().chars().take(400).collect();
    send_bark_text(
        s,
        format!("📬 {}", ctx.email.subject),
        format!("{}\n{}", short_sender(&ctx.email.sender), preview),
        detail_link(ctx.public_url, &ctx.email.id),
    )
    .await
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

// ===========================================================================
// 故障告警
// ===========================================================================

/// 收信失败的原因分类。
///
/// 分类的意义：告警消息里要让人一眼看出「是配置问题、容量问题还是后端故障」，
/// 而不是丢一段难读的 Debug 输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundFailure {
    /// 邮件体积超过 MAX_MESSAGE_BYTES
    TooLarge,
    /// 解析失败（畸形邮件）
    Unparsable,
    /// D1 或 R2 等后端写入异常
    StorageError,
}

impl InboundFailure {
    fn label(self) -> &'static str {
        match self {
            Self::TooLarge => "邮件过大",
            Self::Unparsable => "解析失败",
            Self::StorageError => "存储异常",
        }
    }

    /// 这类失败是否值得立即告警。
    ///
    /// 当前三种都值得告警；保留这个方法是为了将来加入「无需打扰用户」的
    /// 分类（例如重复投递）时有统一入口。
    fn should_alert(self) -> bool {
        true
    }
}

/// 收信失败时推送一条告警，避免「静默丢信」。
///
/// ## 为什么只推给已启用的渠道
/// 告警本身也依赖 Telegram/Bark 配置。如果两个渠道都没开，就只写日志——
/// 这种情况下本来也没有可用的通知手段。
///
/// ## 为什么告警不计入正常推送的限流
/// 告警是低频事件（只在实际出错时触发），不需要额外限流。
/// 但如果 D1 完全不可用，`load_settings` 也会失败，此时同样只写日志。
pub async fn alert_inbound_failure(
    db: &worker::D1Database,
    failure: InboundFailure,
    detail: &str,
    envelope_from: &str,
    envelope_to: &str,
    raw_size: f64,
) {
    if !failure.should_alert() {
        return;
    }

    let settings = match crate::config::load_settings(db).await {
        Ok(s) => s,
        Err(e) => {
            console_error!("[mailflare] 告警失败：无法读取推送配置 {e:?}");
            return;
        }
    };
    if !settings.telegram_enabled && !settings.bark_enabled {
        return;
    }

    // 截断过长的错误信息，避免超出消息体上限。
    let detail = truncate_utf8(detail.to_string(), 400);
    let size_mb = raw_size / 1024.0 / 1024.0;

    let tg_text = format!(
        "*⚠️ MailFlare 收信告警*\n\n*类型*：{}\n*发件人*：{}\n*收件人*：{}\n*大小*：{:.2} MB\n\n*详情*：{}",
        escape_markdown_v2(failure.label()),
        escape_markdown_v2(envelope_from),
        escape_markdown_v2(envelope_to),
        size_mb,
        escape_markdown_v2(&detail),
    );
    let bark_body = format!(
        "{}\n发件人：{}\n大小：{:.2} MB\n{}",
        failure.label(),
        envelope_from,
        size_mb,
        detail
    );

    let (tg, bark) = futures_util::join!(
        async {
            if settings.telegram_enabled {
                send_telegram_text(&settings, truncate_utf8(tg_text, TG_TEXT_LIMIT)).await
            } else {
                ChannelResult::Skipped
            }
        },
        async {
            if settings.bark_enabled {
                send_bark_text(&settings, "⚠️ MailFlare 收信告警".to_string(), bark_body, None)
                    .await
            } else {
                ChannelResult::Skipped
            }
        }
    );

    for (name, result) in [("telegram", &tg), ("bark", &bark)] {
        match result {
            ChannelResult::Failed(msg) => {
                console_error!("[mailflare] {name} 告警推送失败：{msg}")
            }
            ChannelResult::Ok => console_log!("[mailflare] {name} 告警已发送：{}", failure.label()),
            ChannelResult::Skipped => {}
        }
    }
}
