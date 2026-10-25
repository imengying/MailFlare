//! 新邮件推送：Telegram Bot 与 Bark。
//!
//! 跑在 `wait_until()` 里，两个渠道并发、各自带超时，失败只记日志。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use worker::{console_error, console_log, Fetch, Method, Request, RequestInit, Url};

use crate::config::Settings;
use crate::util::truncate_utf8;

/// 单次推送的网络超时。
const PUSH_TIMEOUT: Duration = Duration::from_secs(8);
/// Telegram 单条消息上限 4096 字符，这里留出转义/链接的余量。
const TG_TEXT_LIMIT: usize = 3500;

/// 推送摘要。不复用 `EmailDetail`：正文可能几百 KB，通知只要一小段预览。
#[derive(Debug, Clone)]
pub struct PushEmail {
    pub id: String,
    pub sender: String,
    pub subject: String,
    /// 提取到的验证码。推送只为验证码服务，没有它就不会走到这里。
    pub code: crate::otp::Code,
}

/// 推送所需的上下文。
pub struct PushContext<'a> {
    pub settings: &'a Settings,
    pub email: &'a PushEmail,
}

#[derive(Debug)]
pub enum ChannelResult {
    /// 未启用
    Skipped,
    Ok,
    Failed(String),
}

/// 压平换行，避免日志串行。
fn flatten(e: impl std::fmt::Display) -> String {
    e.to_string().replace('\n', " ")
}

/// `fetch` 没有超时参数，用 `AbortController` + `Delay` 竞速。
async fn fetch_with_timeout(
    url: Url,
    init: RequestInit,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let controller = worker::AbortController::default();
    // 必须把 signal 真正挂到请求上，否则 abort() 只是空操作。
    // `worker::RequestInit` 没有 signal 字段，所以走 `send_with_signal`。
    let signal = controller.signal();
    let request = Fetch::Request(Request::new_with_init(url.as_str(), &init).map_err(flatten)?);

    let send = async move {
        let mut resp = request.send_with_signal(&signal).await.map_err(flatten)?;
        let status = resp.status_code();
        // 失败时把「谁拦的」也带出来：403 可能来自渠道本身，也可能来自
        // 中间的 Cloudflare 风控。`cf-mitigated` / `server` 能直接区分。
        let mut hint = String::new();
        let h = resp.headers();
        for key in ["cf-mitigated", "server", "cf-ray"] {
            if let Ok(Some(v)) = h.get(key) {
                hint.push_str(&format!(" [{key}={v}]"));
            }
        }
        // 读掉 body：一方面归还连接池，另一方面失败时把服务端的原因带回去。
        let detail = resp.text().await.unwrap_or_default();
        Ok::<(u16, String), String>((status, format!("{hint}{detail}")))
    };

    let timeout_fut = async {
        worker::Delay::from(timeout).await;
        Err::<(u16, String), String>("请求超时".to_string())
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

/// 保留字符不加反斜杠会被 Telegram 以 400 拒收。
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

/// 构造 Telegram 消息文本（已转义）。
///
/// 推送只为验证码服务，所以正文只保留「谁发的、什么主题」这类定位信息，
/// 不搬运邮件正文——验证码本身才是用户要的东西。
fn telegram_text(ctx: &PushContext<'_>) -> String {
    let d = ctx.email;
    let mut text = String::with_capacity(320);

    text.push_str(&format!(
        "🔑 *{}：`{}`*\n\n",
        d.code.label,
        escape_markdown_v2(&d.code.value)
    ));

    let name = short_sender(&d.sender);
    text.push_str(&format!("*来自*：{}\n", escape_markdown_v2(name)));
    text.push_str(&format!("*主题*：{}\n", escape_markdown_v2(&d.subject)));

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
    // 与 Bark 一致：补普通客户端该有的头，避免被中间层当成脚本。
    let _ = headers.set("user-agent", "MailFlare/1.0 (+https://github.com/imengying/MailFlare)");
    let _ = headers.set("accept", "application/json");

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body)));

    fetch_with_timeout(url, init, PUSH_TIMEOUT)
        .await
        .map(|(status, _)| status)
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
}

/// 发送一条 Bark 消息。同样抽成原语供告警复用。
async fn send_bark_text(settings: &Settings, title: String, body: String) -> ChannelResult {
    if settings.bark_device_key.is_empty() {
        return ChannelResult::Failed("Bark device key 未配置".into());
    }

    // 留一份原文：POST 被中间层拦截时要拿它走 GET 重试。
    let (title_orig, body_orig) = (title.clone(), body.clone());
    let payload = BarkPayload {
        device_key: &settings.bark_device_key,
        title,
        body,
        group: "MailFlare",
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
    // Bark 官方服务挂在 Cloudflare 后面，而 Worker 的默认 UA（空或
    // `Cloudflare-Workers`）容易被对方的 WAF 判成自动化请求，直接回 403
    // 拦截页（响应体是一段 HTML，不是正经的 Bark 错误）。
    // 这里补齐普通 HTTP 客户端该有的头，把请求伪装成正常调用。
    let _ = headers.set("user-agent", "MailFlare/1.0 (+https://github.com/imengying/MailFlare)");
    let _ = headers.set("accept", "application/json, */*");
    let _ = headers.set("accept-language", "zh-CN,zh;q=0.9,en;q=0.8");

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body)));

    match fetch_with_timeout(url, init, PUSH_TIMEOUT).await {
        Ok((200..=299, _)) => ChannelResult::Ok,
        // 403 且响应体是 HTML：基本可以确定是中间的 Cloudflare 拦的，
        // 不是 Bark 在拒绝。换 GET（Bark 官方主推的形式）再试一次——
        // 它的路径参数走的是完全不同的一条路由，能绕过针对 POST 的规则。
        Ok((403, detail)) if detail.contains("<!DOCTYPE") || detail.contains("no-js") => {
            console_log!("[mailflare] Bark POST 被中间层拦截，降级为 GET 重试");
            send_bark_get(settings, &title_orig, &body_orig).await
        }
        Ok((code, detail)) => {
            let detail = detail.trim();
            if detail.is_empty() {
                ChannelResult::Failed(format!("Bark 返回 HTTP {code}"))
            } else {
                // 只取前 160 个字符：错误详情不需要全文，也不要带截断提示语。
                let short: String = detail.chars().take(160).collect();
                ChannelResult::Failed(format!("Bark 返回 HTTP {code}：{short}"))
            }
        }
        Err(e) => ChannelResult::Failed(e),
    }
}

/// Bark 的 GET 形式：`{server}/{device_key}/{title}/{body}`。
///
/// 作为 POST 被中间层拦截时的备用通道。官方文档两种都支持，
/// GET 的参数在路径里，绕开了针对 JSON 请求体的风控规则。
async fn send_bark_get(settings: &Settings, title: &str, body: &str) -> ChannelResult {
    let title = crate::util::percent_encode(title);
    let body = crate::util::percent_encode(body);
    let Ok(url) = Url::parse(&format!(
        "{}/{}/{}/{}",
        settings.bark_server,
        crate::util::percent_encode(&settings.bark_device_key),
        title,
        body
    )) else {
        return ChannelResult::Failed("Bark 服务地址非法".into());
    };

    let headers = worker::Headers::new();
    let _ = headers.set("user-agent", "MailFlare/1.0 (+https://github.com/imengying/MailFlare)");
    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);

    match fetch_with_timeout(url, init, PUSH_TIMEOUT).await {
        Ok((200..=299, _)) => ChannelResult::Ok,
        Ok((code, detail)) => {
            let short: String = detail.trim().chars().take(160).collect();
            if short.is_empty() {
                ChannelResult::Failed(format!("Bark 返回 HTTP {code}（GET 重试也失败）"))
            } else {
                ChannelResult::Failed(format!("Bark 返回 HTTP {code}（GET 重试）：{short}"))
            }
        }
        Err(e) => ChannelResult::Failed(format!("{e}（GET 重试）")),
    }
}

/// 验证码的 Telegram 通知。
async fn push_telegram(ctx: &PushContext<'_>) -> ChannelResult {
    if !ctx.settings.telegram_enabled {
        return ChannelResult::Skipped;
    }
    send_telegram_text(ctx.settings, telegram_text(ctx)).await
}

/// 验证码的 Bark 通知。
async fn push_bark(ctx: &PushContext<'_>) -> ChannelResult {
    let s = ctx.settings;
    if !s.bark_enabled {
        return ChannelResult::Skipped;
    }

    let code = &ctx.email.code;
    send_bark_text(
        s,
        format!("🔑 {} {}", code.label, code.value),
        format!("{}\n{}", short_sender(&ctx.email.sender), ctx.email.subject),
    )
    .await
}

/// 测试推送用的文本。
///
/// 刻意复用真实格式（`🔑 验证码：xxx`），发出去看到的样子就是真邮件推送的样子；
/// 验证码用固定值，一眼能认出这是测试消息。
fn test_telegram_text() -> String {
    format!(
        "🔑 *{}：`{}`*\n\n*来自*：{}\n*主题*：{}",
        "验证码",
        "123456",
        escape_markdown_v2("MailFlare 测试"),
        escape_markdown_v2("这是一条测试推送，收到即表示配置正确")
    )
}

/// 测试推送：给每个已启用的渠道发一条固定消息，并回报各自结果。
///
/// 和 `dispatch` 的区别：这里**不做验证码过滤**，也不并发——串行更好定位
/// 到底是哪个渠道失败。返回明细给设置页展示，用户不用去翻日志。
pub async fn test_push(settings: &Settings) -> (ChannelResult, ChannelResult) {
    let telegram = if settings.telegram_enabled {
        send_telegram_text(settings, test_telegram_text()).await
    } else {
        ChannelResult::Skipped
    };
    let bark = if settings.bark_enabled {
        send_bark_text(
            settings,
            "🔑 验证码 123456".to_string(),
            "MailFlare 测试推送\n收到即表示配置正确".to_string(),
        )
        .await
    } else {
        ChannelResult::Skipped
    };
    (telegram, bark)
}

/// 最近一次推送结果的持久化形态。
///
/// 存进 D1 的意义：Worker 日志在网页上看不到，用户只会觉得「没收到」。
/// 把最后一次的结果落到 `configs`，设置页就能直接显示失败原因。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PushLog {
    /// 秒级时间戳。
    pub at: i64,
    /// 触发的邮件主题，方便对上是哪一封。
    pub subject: String,
    pub code: String,
    /// 每个渠道的结果，形如 `["telegram: ok", "bark: HTTP 400"]`。
    pub channels: Vec<String>,
    /// 是否全部成功。
    pub ok: bool,
}

/// 把一次推送的结果写进 D1（失败只记日志，不影响收信）。
pub async fn record_push_log(db: &worker::D1Database, log: &PushLog) {
    let Ok(json) = serde_json::to_string(log) else {
        return;
    };
    if let Err(e) = crate::config::put_raw(db, crate::config::keys::LAST_PUSH, &json).await {
        console_error!("[mailflare] 写入推送状态失败: {e:?}");
    }
}

/// 读取最近一次推送结果。
pub async fn load_push_log(db: &worker::D1Database) -> Option<PushLog> {
    let raw = crate::config::get_raw(db, crate::config::keys::LAST_PUSH)
        .await
        .ok()??;
    serde_json::from_str(&raw).ok()
}

/// 并发推送两个渠道，返回每个渠道的可读结果。
pub async fn dispatch(ctx: &PushContext<'_>) -> Vec<String> {
    let (telegram, bark) = futures_util::join!(push_telegram(ctx), push_bark(ctx));

    let mut lines = Vec::new();
    // 日志带上邮件 id，便于区分「没推」和「推失败」。
    for (name, r) in [("telegram", &telegram), ("bark", &bark)] {
        match r {
            ChannelResult::Failed(msg) => {
                console_error!("[mailflare] {name} 推送失败 ({}): {msg}", ctx.email.id);
                lines.push(format!("{name}: {msg}"));
            }
            ChannelResult::Ok => {
                console_log!("[mailflare] {name} 推送成功 ({})", ctx.email.id);
                lines.push(format!("{name}: ok"));
            }
            ChannelResult::Skipped => {}
        }
    }
    lines
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
    /// 邮件体积超过「单封邮件上限」
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
                send_bark_text(&settings, "⚠️ MailFlare 收信告警".to_string(), bark_body)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otp::Code;

    fn email() -> PushEmail {
        PushEmail {
            id: "id1".into(),
            sender: "GitHub <noreply@github.com>".into(),
            subject: "Your verification code".into(),
            code: Code {
                value: "482913".into(),
                label: "验证码",
            },
        }
    }

    fn ctx<'a>(settings: &'a Settings, email: &'a PushEmail) -> PushContext<'a> {
        PushContext {
            settings,
            email,
        }
    }

    #[test]
    fn telegram_puts_code_first() {
        let settings = Settings::default();
        let mail = email();
        let text = telegram_text(&ctx(&settings, &mail));
        let code_at = text.find("482913").expect("应包含验证码");
        let from_at = text.find("来自").expect("应包含来源");
        assert!(code_at < from_at, "验证码应置顶: {text}");
    }

    #[test]
    fn telegram_carries_no_body_preview() {
        // 验证码通知只做定位，不搬运正文。
        let settings = Settings::default();
        let text = telegram_text(&ctx(&settings, &email()));
        assert!(!text.contains("新邮件"), "不应再有普通邮件标题");
    }

    #[test]
    fn code_is_markdown_escaped() {
        let settings = Settings::default();
        // 下划线属于 MarkdownV2 保留字符，必须被转义，否则 Telegram 会拒收。
        let mut mail = email();
        mail.code = Code {
            value: "AB_12".into(),
            label: "验证码",
        };
        let text = telegram_text(&ctx(&settings, &mail));
        assert!(text.contains("AB\\_12"), "实际: {text}");
    }
}
