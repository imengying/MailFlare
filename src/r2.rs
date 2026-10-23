//! R2 存储层：通过 **S3 API**（`fetch` + SigV4）读写附件。
//!
//! 邮件正文、发件人、主题、附件元数据全部在 D1 的 `emails` 表里；
//! R2 只承担大块的二进制（附件），这样 D1 单行不会因为附件而膨胀。
//!
//! ## 为什么走 S3 API 而不是 Worker 绑定
//! 绑定（`[[r2_buckets]]`）要求桶名写进 `wrangler.toml`，而桶是用户自己的
//! 资源——名字、账号、用哪个桶都应该由用户在后台决定。改用 S3 API 后，
//! 端点、密钥、桶名全部来自设置表，改完即时生效，不需要重新部署。
//!
//! ## key 布局
//! ```text
//! mail/att/{email_id}/{index}      附件（index 为 MIME 部件序号）
//! ```
//! 统一带 `mail/` 前缀，便于和其他用途共用同一个桶。
//!
//! ## 零拷贝
//! 下载时**绝不**调用 `bytes().await`：附件可能有几十 MB，读进 WASM 线性
//! 内存既慢又会顶到 128MB 上限。正确做法是从上游响应里取出
//! `ResponseBody::Stream` 直接交给运行时转发，字节全程不经过 Rust 堆。

use worker::{console_error, Fetch, Headers, Method, Request, RequestInit, Response};

use crate::config::Settings;
use crate::db::AttachmentMeta;
use crate::error::{AppError, AppResult};
use crate::mail::RawAttachment;
use crate::s3sign::{self, SignRequest};
use crate::util::{ascii_filename, now_ms, percent_encode};

/// 对象 key 的统一前缀。
const KEY_PREFIX: &str = "mail";

/// 附件对象 key：`mail/att/{email_id}/{index}`。
#[inline]
pub fn attachment_key(email_id: &str, index: usize) -> String {
    format!("{KEY_PREFIX}/att/{email_id}/{index}")
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
/// 只接受 `[A-Za-z0-9-]+_[0-9]+` 这一种形态。严格校验既能让调用方尽早拿到
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
    // 挡掉溢出。
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

/// 校验 R2 配置是否完整，缺项时给出明确提示（而不是让 fetch 403 让人猜）。
fn require_r2(settings: &Settings) -> AppResult<&Settings> {
    if !settings.r2_configured() {
        return Err(AppError::Internal(
            "R2 未配置完整，请在「设置 → R2 存储」填写账号 ID、Access Key、Secret Key 与桶名".into(),
        ));
    }
    Ok(settings)
}

/// R2 的 S3 端点主机名。
fn endpoint_host(settings: &Settings) -> String {
    format!("{}.r2.cloudflarestorage.com", settings.r2_account_id)
}

/// 构造一个已签名的 S3 请求所需的信息。
struct SignedUrl {
    /// 完整 URL（含协议与桶名前缀的路径）。
    url: String,
    /// 参与签名的额外头（`Authorization` / `x-amz-*`）。
    headers: Headers,
}

/// 为一次 S3 请求生成 URL 与签名头。
///
/// `extra_headers` 是需要**参与签名**的附加头（例如 `content-type`）；
/// 不参与签名的头（`Range`）由调用方自己加，不要塞这里。
fn sign(
    settings: &Settings,
    method: &str,
    key: &str,
    payload_hash: &str,
    extra_headers: &[(&str, &str)],
) -> AppResult<SignedUrl> {
    let host = endpoint_host(settings);
    // 路径固定为 `/{bucket}/{key}`；桶名与 key 都要按 SigV4 规则编码。
    let canonical_uri = s3sign::encode_uri_path(&format!("/{}/{key}", settings.r2_bucket));

    let (date, amz_date) = s3sign::aws_timestamps(now_ms());

    // 参与签名的头部：host + x-amz-* + 调用方指定的。
    let mut pairs: Vec<(&str, &str)> = Vec::with_capacity(3 + extra_headers.len());
    pairs.push(("host", host.as_str()));
    pairs.push(("x-amz-content-sha256", payload_hash));
    pairs.push(("x-amz-date", amz_date.as_str()));
    pairs.extend_from_slice(extra_headers);

    let auth = s3sign::authorization(&SignRequest {
        method,
        canonical_uri: &canonical_uri,
        headers: &pairs,
        payload_hash,
        access_key: &settings.r2_access_key_id,
        secret: &settings.r2_secret_access_key,
        date: &date,
        amz_date: &amz_date,
    });

    let headers = Headers::new();
    for (name, value) in &pairs {
        // host 由 fetch 自动设置，手动写入会被运行时拒绝。
        if *name != "host" {
            headers.set(name, value)?;
        }
    }
    headers.set("authorization", &auth)?;

    Ok(SignedUrl {
        url: format!("https://{host}{canonical_uri}"),
        headers,
    })
}

/// 把附件写入 R2。
///
/// **按值接收** `att`：`att.bytes`（可能几 MB）直接作为请求体，不会在
/// 内部再 clone 一份。这一点很关键——一封 10MB 附件若被复制，
/// 在 128MB 上限下就是致命的。
pub async fn put_attachment(
    settings: &Settings,
    key: &str,
    att: RawAttachment,
    email_id: &str,
) -> AppResult<()> {
    let settings = require_r2(settings)?;
    // `cid` 是内嵌资源的标识，只用于正文改写，写 R2 时用不上。
    let RawAttachment {
        content_type,
        bytes,
        filename,
        inline,
        ..
    } = att;

    // PUT 的载荷哈希必须真实计算，否则 R2 会以 403 拒绝。
    let payload_hash = s3sign::sha256_hex(&bytes);

    // 这些头部需要参与签名，才能保证不被中途篡改。
    let disposition = content_disposition(&filename, inline);
    let safe_content_type = sanitize_content_type(&content_type);
    // 自定义元数据值必须是 ASCII；中文文件名做一次百分号编码。
    let encoded_name = percent_encode(&filename);

    let signed = sign(
        settings,
        "PUT",
        key,
        &payload_hash,
        &[
            ("content-type", safe_content_type.as_str()),
            ("content-disposition", disposition.as_str()),
            ("x-amz-meta-email-id", email_id),
            ("x-amz-meta-filename", encoded_name.as_str()),
        ],
    )?;

    // 内容长度显式声明，避免 fetch 用 chunked 编码（S3 对 PUT 更偏好定长）。
    signed
        .headers
        .set("content-length", &bytes.len().to_string())?;

    let mut init = RequestInit::new();
    init.with_method(Method::Put)
        .with_headers(signed.headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from(
            worker::js_sys::Uint8Array::from(bytes.as_slice()),
        )));

    let request = Request::new_with_init(&signed.url, &init)?;
    let mut resp = Fetch::Request(request).send().await?;

    if resp.status_code() >= 300 {
        // 不读 body 就无法知道 R2 的具体错误（例如 NoSuchBucket、
        // SignatureDoesNotMatch），排障时这几百字节非常值得。
        let detail = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "R2 上传失败 HTTP {}：{}",
            resp.status_code(),
            detail.trim()
        )));
    }
    Ok(())
}

/// 发起一次已签名的下载请求，把上游响应原样交回调用方。
///
/// `range` 是客户端带来的 `Range` 头：它**不参与签名**（SigV4 不要求，
/// 加了反而要求值完全一致才签名成功），直接透传给 S3 即可。
///
/// 返回 `None` 表示对象不存在（404）。
pub async fn get_attachment(
    settings: &Settings,
    key: &str,
    range: Option<&str>,
) -> AppResult<Option<Response>> {
    let settings = require_r2(settings)?;
    let signed = sign(
        settings,
        "GET",
        key,
        s3sign::EMPTY_PAYLOAD_SHA256,
        &[],
    )?;

    if let Some(range) = range {
        signed.headers.set("range", range)?;
    }

    let mut init = RequestInit::new();
    init.with_method(Method::Get)
        .with_headers(signed.headers);

    let request = Request::new_with_init(&signed.url, &init)?;
    let resp = Fetch::Request(request).send().await?;

    match resp.status_code() {
        200 | 206 => Ok(Some(resp)),
        404 => Ok(None),
        other => Err(AppError::Internal(format!(
            "R2 读取失败 HTTP {other}（key={key}）"
        ))),
    }
}

/// 删除某封邮件的全部 R2 附件对象。
///
/// S3 的批量删除需要 XML 请求体，这里逐个 `DELETE` 更简单，而且附件数
/// 通常只有个位数。失败只记日志，不让已经删掉的 D1 记录回滚
/// （孤对象可由桶的生命周期规则兜底清理）。
pub async fn delete_email_objects(settings: &Settings, attachments: &[AttachmentMeta]) {
    // `stored = true` 的附件在入库时一定写入了 key（见 inbound 的 process），
    // 所以这里直接用记录里的 key，不需要再按约定推导。
    let keys: Vec<&str> = attachments
        .iter()
        .filter(|att| att.stored && !att.key.is_empty())
        .map(|att| att.key.as_str())
        .collect();
    if keys.is_empty() {
        return;
    }

    // 配置不全时无法删除，记一条日志就够——邮件已经不在列表里了。
    if !settings.r2_configured() {
        console_error!("[mailflare] R2 未配置，无法清理 {} 个附件对象", keys.len());
        return;
    }

    for key in keys {
        if let Err(e) = delete_object(settings, key).await {
            console_error!("[mailflare] 删除 R2 对象失败（{key}）: {e:?}");
        }
    }
}

/// 删除单个对象。
async fn delete_object(settings: &Settings, key: &str) -> AppResult<()> {
    let signed = sign(
        settings,
        "DELETE",
        key,
        s3sign::EMPTY_PAYLOAD_SHA256,
        &[],
    )?;

    let mut init = RequestInit::new();
    init.with_method(Method::Delete)
        .with_headers(signed.headers);

    let request = Request::new_with_init(&signed.url, &init)?;
    let resp = Fetch::Request(request).send().await?;

    // 404 视为成功：目标状态已经达成（对象不在了）。
    if resp.status_code() >= 300 && resp.status_code() != 404 {
        return Err(AppError::Internal(format!(
            "R2 删除失败 HTTP {}",
            resp.status_code()
        )));
    }
    Ok(())
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

/// 把 MIME 类型清洗成可安全写进请求头/响应头的 ASCII 形式。
///
/// 只保留 `type/subtype`，其它字符一律丢弃；解析不出来就退回
/// `application/octet-stream`（宁可让浏览器下载，也不写出畸形头部）。
pub fn sanitize_content_type(raw: &str) -> String {
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

/// 校验配置里声明的并发度，避免误配导致一次开过多子请求。
pub fn effective_concurrency(settings: &Settings) -> usize {
    settings.r2_concurrency.clamp(1, 16) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_layout_uses_mail_prefix() {
        assert_eq!(attachment_key("abc123", 0), "mail/att/abc123/0");
        assert_eq!(attachment_key("abc123", 12), "mail/att/abc123/12");
    }

    #[test]
    fn object_id_round_trips() {
        let id = attachment_object_id("a1b2c3", 7);
        assert_eq!(parse_attachment_id(&id).unwrap(), ("a1b2c3", 7));
    }

    #[test]
    fn parse_rejects_malformed_ids() {
        for bad in ["", "_", "abc", "abc_", "_0", "abc_x", "abc_-1", "abc_01", "abc_0/../x"] {
            assert!(
                parse_attachment_id(bad).is_err(),
                "应该拒绝: {bad:?}"
            );
        }
    }

    #[test]
    fn sanitize_content_type_falls_back() {
        assert_eq!(sanitize_content_type("image/png"), "image/png");
        assert_eq!(sanitize_content_type("image/png; charset=x"), "image/png");
        assert_eq!(sanitize_content_type("图片/png"), "application/octet-stream");
        assert_eq!(sanitize_content_type(""), "application/octet-stream");
    }

    #[test]
    fn concurrency_is_clamped() {
        let low = Settings { r2_concurrency: -5, ..Default::default() };
        assert_eq!(effective_concurrency(&low), 1);
        let high = Settings { r2_concurrency: 999, ..Default::default() };
        assert_eq!(effective_concurrency(&high), 16);
    }
}
