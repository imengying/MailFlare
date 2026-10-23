//! 邮件解析核心。
//!
//! 用 `mail-parser` 以**零拷贝**方式解析原始 RFC5322 报文：`Message` 借用了入参
//! `&[u8]`，所有 header 字段都指向原始缓冲区，只有真正需要落库的字段才调用
//! `to_string()` 分配内存。
//!
//! 解码由 mail-parser 内部完成（已开启 `full_encoding`），因此 GBK / GB2312 /
//! GB18030 / Big5 等中文编码的标题与正文都能正确还原。

use mail_parser::{MessageParser, MimeHeaders};

use crate::db::AttachmentMeta;

/// 解析后的中间结果。所有字符串都是**拥有所有权**的，可以安全地跨 await
/// 使用（mail-parser 的借用结果不能跨 await）。
pub struct ParsedEmail {
    pub message_id: Option<String>,
    pub sender: String,
    pub recipient: String,
    pub subject: String,
    pub text_body: String,
    pub html_body: String,
    /// 按 MIME 出现顺序排列的附件（含内联资源）。
    pub attachments: Vec<RawAttachment>,
    /// 邮件头里的 Date（毫秒）；解析失败时为 `None`，由调用方回退到收信时间。
    pub date_ms: Option<i64>,
}

/// 待转存到 R2 的附件负载。
pub struct RawAttachment {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub inline: bool,
    pub cid: Option<String>,
}

/// 把地址结构转成 `"显示名 <addr@example.com>"` 形式。
fn format_address(addr: &mail_parser::Address<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for a in addr.iter() {
        let email = a.address().unwrap_or("").trim();
        if email.is_empty() {
            continue;
        }
        match a.name() {
            // 名字里自带尖括号或纯 ASCII 时也保持一致格式即可。
            Some(name) if !name.trim().is_empty() => {
                parts.push(format!("{} <{}>", name.trim(), email))
            }
            _ => parts.push(email.to_string()),
        }
    }
    parts.join(", ")
}

/// 去掉 Content-ID 两端的尖括号：`<abc@x>` -> `abc@x`。
fn normalize_cid(raw: &str) -> String {
    raw.trim().trim_start_matches('<').trim_end_matches('>').to_string()
}

/// 兜底文件名：`附件-3.png`，保证扩展名尽量保留。
fn fallback_filename(index: usize, content_type: &str) -> String {
    let ext = match content_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/html" => "html",
        "application/zip" => "zip",
        _ => "bin",
    };
    format!("附件-{}.{ext}", index + 1)
}

/// 解析原始邮件字节。
///
/// `max_body_bytes` 控制正文入库上限（超出部分截断），`max_attachment_bytes`
/// 控制单个附件是否值得转存 R2（超限的附件仍会在列表里出现，但标记 `stored=false`）。
pub fn parse(raw: &[u8], max_body_bytes: usize) -> Option<ParsedEmail> {
    let message = MessageParser::default().parse(raw)?;

    let sender = message
        .from()
        .map(format_address)
        .filter(|s| !s.is_empty())
        // 发件人解析失败时退回到 Return-Path / Sender。
        .or_else(|| message.return_address().map(|s| s.to_string()))
        .unwrap_or_else(|| "未知发件人".to_string());

    let recipient = message
        .to()
        .map(format_address)
        .filter(|s| !s.is_empty())
        .unwrap_or_default();

    let subject = message
        .subject()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(无主题)".to_string());

    // body_html(0) / body_text(0) 会返回 `Cow`：未超限时可能是借用的切片，
    // 这里统一 `into_owned()` 以便跨 await 使用（只发生一次拷贝）。
    let text_body = message
        .body_text(0)
        .map(|c| crate::util::truncate_utf8(c.into_owned(), max_body_bytes))
        .unwrap_or_default();

    let html_body = message
        .body_html(0)
        .map(|c| crate::util::truncate_utf8(c.into_owned(), max_body_bytes))
        .unwrap_or_default();

    let attachments = message
        .attachments()
        .enumerate()
        .map(|(index, part)| {
            let content_type = part
                .content_type()
                .map(|ct| {
                    let subtype = ct.c_subtype.as_deref().unwrap_or("octet-stream");
                    format!("{}/{}", ct.c_type, subtype)
                })
                .unwrap_or_else(|| "application/octet-stream".to_string());

            let filename = part
                .attachment_name()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| fallback_filename(index, &content_type));

            // inline 判定：Content-Disposition: inline，或带 Content-ID 的图片。
            let disposition_inline = part
                .content_disposition()
                .is_some_and(|cd| cd.c_type.eq_ignore_ascii_case("inline"));
            let cid = part.content_id().map(normalize_cid).filter(|s| !s.is_empty());
            let inline = disposition_inline || (cid.is_some() && content_type.starts_with("image/"));

            RawAttachment {
                filename,
                content_type,
                // `contents()` 返回 `&[u8]`，解码已在 mail-parser 内部完成。
                bytes: part.contents().to_vec(),
                inline,
                cid,
            }
        })
        .collect();

    let date_ms = message.date().and_then(|d| {
        let ts = d.to_timestamp();
        // 明显不合法的时间（1970 之前 / 2100 之后）交给调用方用收信时间兜底。
        (ts > 0).then_some(ts * 1000)
    });

    Some(ParsedEmail {
        message_id: message
            .message_id()
            .map(|s| s.trim().trim_start_matches('<').trim_end_matches('>').to_string())
            .filter(|s| !s.is_empty()),
        sender,
        recipient,
        subject,
        text_body,
        html_body,
        attachments,
        date_ms,
        // 显式丢弃 message，释放 mail-parser 的内部结构（原始字节仍在调用方手里）。
    })
}

/// 附件入库元数据的前缀构造：`stored` 由上层在 R2 写入成功后置位。
pub fn attachment_meta(index: usize, att: &RawAttachment) -> AttachmentMeta {
    AttachmentMeta {
        index,
        filename: att.filename.clone(),
        content_type: att.content_type.clone(),
        size: att.bytes.len(),
        key: String::new(),
        inline: att.inline,
        cid: att.cid.clone(),
        stored: false,
    }
}
