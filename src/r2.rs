//! R2 存储层：原始 `.eml` 与附件的写入 / 流式读取 / 删除。
//!
//! R2 key 布局（刻意用纯 ASCII，避免中文文件名带来的编码问题）：
//! ```text
//! raw/{email_id}.eml          原始邮件
//! att/{email_id}/{index}      附件（index 为 MIME 部件序号）
//! ```
//! 真实文件名保存在 D1 的 `emails.attachments` JSON 里，下载时写入
//! `Content-Disposition`。

use worker::{Bucket, HttpMetadata, Object};

use crate::config::Config;
use crate::db::AttachmentMeta;
use crate::error::{AppError, AppResult};
use crate::mail::RawAttachment;
use crate::util::{ascii_filename, percent_encode};

/// 原始邮件对象 key。
#[inline]
pub fn raw_key(email_id: &str) -> String {
    format!("raw/{email_id}.eml")
}

/// 附件对象 key。
#[inline]
pub fn attachment_key(email_id: &str, index: usize) -> String {
    format!("att/{email_id}/{index}")
}

/// 附件下载 ID：`{email_id}_{index}`。
///
/// email_id 是纯十六进制，因此用最后一个 `_` 切分不会有歧义。
#[inline]
pub fn attachment_object_id(email_id: &str, index: usize) -> String {
    format!("{email_id}_{index}")
}

/// 解析附件下载 ID。
///
/// 只接受 `[A-Za-z0-9]+_[0-9]+` 这一种形态。严格校验既能让调用方尽早拿到
/// 400（而不是拿一个乱七八糟的字符串去查 D1），也顺手挡掉了路径穿越之类的
/// 试探输入。
pub fn parse_attachment_id(raw: &str) -> AppResult<(&str, usize)> {
    const ERR: &str = "附件 ID 格式错误";

    let Some((email_id, index)) = raw.rsplit_once('_') else {
        return Err(AppError::BadRequest(ERR.into()));
    };

    let valid_id = !email_id.is_empty()
        && email_id.len() <= 64
        && email_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    // 序号必须是不带前导零的十进制数字（`0` 本身合法）；限制长度顺便
    // 挡掉 `u32`/`usize` 溢出。
    let valid_index = !index.is_empty()
        && index.len() <= 9
        && (index == "0" || !index.starts_with('0'))
        && index.bytes().all(|b| b.is_ascii_digit());

    if !valid_id || !valid_index {
        return Err(AppError::BadRequest(ERR.into()));
    }

    let index: usize = index.parse().map_err(|_| AppError::BadRequest(ERR.into()))?;
    Ok((email_id, index))
}

/// 把附件写入 R2。
///
/// **按值接收** `att`：`att.bytes`（可能是几 MB）会被直接 move 进 R2 的
/// `Data::Bytes`。如果这里传引用、在内部 `clone`，一封 10MB 附件就会额外
/// 占用 10MB WASM 堆——在 128MB 上限下这很致命。
pub async fn put_attachment(
    bucket: &Bucket,
    key: &str,
    att: RawAttachment,
    email_id: &str,
) -> AppResult<()> {
    // R2 的 customMetadata 值必须是 ASCII，中文文件名做一次百分号编码。
    let mut meta = std::collections::HashMap::with_capacity(2);
    meta.insert("email-id".to_string(), email_id.to_string());
    meta.insert("filename".to_string(), percent_encode(&att.filename));

    let disposition = content_disposition(&att.filename, att.inline);
    let RawAttachment {
        content_type,
        bytes,
        ..
    } = att;

    bucket
        .put(key, bytes)
        .http_metadata(HttpMetadata {
            content_type: Some(content_type),
            content_disposition: Some(disposition),
            ..Default::default()
        })
        .custom_metadata(meta)
        .execute()
        .await?;

    Ok(())
}

/// 写原始 `.eml` 到 R2。
///
/// 入参按值传入：`Vec<u8>` 会被直接转成 R2 的 `Data::Bytes`，
/// 省掉一次 Rust 侧的大块内存拷贝。
pub async fn put_raw_eml(bucket: &Bucket, email_id: &str, raw: Vec<u8>) -> AppResult<()> {
    bucket
        .put(raw_key(email_id), raw)
        .http_metadata(HttpMetadata {
            content_type: Some("message/rfc822".to_string()),
            content_disposition: Some(format!(
                "attachment; filename=\"{}.eml\"",
                ascii_filename(email_id)
            )),
            ..Default::default()
        })
        .execute()
        .await?;
    Ok(())
}

/// 读取对象用于流式转发。不存在时返回 `None`。
pub async fn get_object(bucket: &Bucket, key: &str) -> AppResult<Option<Object>> {
    Ok(bucket.get(key).execute().await?)
}

/// 删除某封邮件在 R2 上的全部对象（原始邮件 + 所有附件）。
///
/// 单次 `deleteMultiple` 最多 1000 个 key，这里按批切分；失败只记日志，
/// 不让已经删掉的 D1 记录回滚（孤对象可由生命周期规则清理）。
pub async fn delete_email_objects(
    bucket: &Bucket,
    email_id: &str,
    attachments: &[AttachmentMeta],
    store_raw: bool,
) {
    let mut keys: Vec<String> = Vec::with_capacity(attachments.len() + 1);
    if store_raw {
        keys.push(raw_key(email_id));
    }
    for att in attachments {
        // 优先用记录里的 key；老数据没有 key 时按约定推导。
        if att.stored {
            keys.push(if att.key.is_empty() {
                attachment_key(email_id, att.index)
            } else {
                att.key.clone()
            });
        }
    }
    if keys.is_empty() {
        return;
    }

    for chunk in keys.chunks(1000) {
        // `delete_multiple` 接受 `Vec<impl Deref<Target = str>>`；这里传 &str 切片。
        let batch: Vec<&str> = chunk.iter().map(String::as_str).collect();
        if let Err(e) = bucket.delete_multiple(batch).await {
            worker::console_error!("[mailflare] 批量删除 R2 对象失败: {e}");
        }
    }
}

/// 生成附件的 `Content-Disposition: attachment/inline` 头。
///
/// 同时给出 ASCII 兜底名与 RFC 5987 的 UTF-8 名，兼容所有主流浏览器。
pub fn content_disposition(filename: &str, inline: bool) -> String {
    format!(
        "{}; filename=\"{}\"; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        ascii_filename(filename),
        percent_encode(filename)
    )
}

/// 附件是否允许在页面内联展示（只有图片/PDF 才值得 inline，避免 XSS）。
pub fn inline_allowed(content_type: &str) -> bool {
    matches!(
        content_type.split(';').next().unwrap_or("").trim(),
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/bmp"
            | "image/avif"
            | "application/pdf"
    )
}

/// 校验配置里声明的并发度，避免误配导致一次开过多子请求。
pub fn effective_concurrency(cfg: &Config) -> usize {
    cfg.r2_concurrency.clamp(1, 16)
}
