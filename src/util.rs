//! 与业务无关的小工具：时间、随机 ID、截断、常量时间比较、响应构造。

use serde::Serialize;
use sha2::{Digest, Sha256};
use worker::{Headers, Response, ResponseBuilder};

/// 当前时间（毫秒 Unix 时间戳）。
///
/// 用 `worker::Date` 而不是 `std::time::SystemTime`：后者在
/// wasm32-unknown-unknown 上会 panic。
#[inline]
pub fn now_ms() -> i64 {
    worker::Date::now().as_millis() as i64
}

/// 当前时间（秒）。
#[inline]
pub fn now_secs() -> i64 {
    now_ms() / 1000
}

/// 生成 `n` 字节随机数据。
///
/// `getrandom` 在 wasm32 上走 WebCrypto（Cargo.toml 里已开启 `js` feature）。
/// 极端情况下（例如自建运行时没暴露 crypto）退回 Math.random，
/// 仅作为可用性兜底，不用于加密用途以外的敏感场景会另有说明。
fn random_bytes(buf: &mut [u8]) {
    // getrandom 0.4 把 `getrandom(&mut [u8])` 改名成了 `fill()`。
    if getrandom::fill(buf).is_ok() {
        return;
    }
    for b in buf.iter_mut() {
        // 0..=255 的均匀取值不可靠，但兜底足够生成不冲突的 id。
        *b = (worker::js_sys::Math::random() * 256.0) as u8;
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// 生成邮件主键：13 位十六进制毫秒时间戳 + 16 位十六进制随机数。
///
/// 前缀是时间戳，天然按插入顺序递增，便于 D1 的主键局部性；
/// 32 个字符全部是十六进制，不含 `-`/`.`，可以安全地参与附件复合 ID。
pub fn new_id() -> String {
    let mut rnd = [0u8; 8];
    random_bytes(&mut rnd);
    format!("{:013x}{}", now_ms(), to_hex(&rnd))
}

/// 按**字节**上限截断字符串，并保证落在 UTF-8 字符边界上。
///
/// 邮件正文可能非常大，而 D1 单行有 1MB 上限、WASM 堆只有 128MB，
/// 所以入库前必须截断。已经是合法长度时直接返回，零拷贝。
pub fn truncate_utf8(s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }

    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    // `truncate` 就地丢弃尾部，不再分配新字符串。
    let mut s = s;
    s.truncate(end);
    s.push_str("\n\n…（内容过长，已截断）");
    s
}

/// 常量时间比较两段字节，避免密码比较时泄露长度/内容信息。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 先做 SHA-256 再常量时间比较，这样即使两个密码长度不同，
/// 比较过程消耗的时间也与内容无关。
pub fn secret_eq(candidate: &str, expected: &str) -> bool {
    let a = Sha256::digest(candidate.as_bytes());
    let b = Sha256::digest(expected.as_bytes());
    constant_time_eq(&a, &b)
}

/// RFC 3986 / RFC 5987 的 `attr-char` 百分号编码（用于 `filename*=UTF-8''...`）。
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'!'
            | b'#'
            | b'$'
            | b'&'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~' => out.push(b as char),
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// 把文件名压成仅含 ASCII 的安全版本，用于 `filename="..."` 的历史兼容字段。
pub fn ascii_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "attachment".to_string()
    } else {
        cleaned.chars().take(120).collect()
    }
}

/// 构造 JSON 响应（统一 `no-store`，避免邮件内容被中间层缓存）。
pub fn json_response<T: Serialize>(status: u16, value: &T) -> Response {
    let headers = Headers::new();
    let _ = headers.set("cache-control", "no-store");
    let _ = headers.set("x-content-type-options", "nosniff");

    match ResponseBuilder::new()
        .with_headers(headers)
        .from_json(value)
    {
        Ok(resp) => resp.with_status(status),
        Err(e) => {
            worker::console_error!("[mailflare] serialize response failed: {e}");
            // 同上：`ResponseBuilder::empty()` 不会失败，用它兜底绝无 panic。
            worker::ResponseBuilder::new().with_status(500).empty()
        }
    }
}

/// 统一的成功响应包装：`{"ok":true,"data":...}`。
pub fn ok_json<T: Serialize>(data: &T) -> Response {
    #[derive(Serialize)]
    struct Wrapper<'a, T> {
        ok: bool,
        data: &'a T,
    }
    json_response(200, &Wrapper { ok: true, data })
}

/// 从 URL query 里取一个参数（避免为此引入 serde_urlencoded）。
pub fn query_param(url: &url::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// 解析整型 query 参数并夹在 `[min, max]` 区间内。
pub fn query_i64(url: &url::Url, key: &str, default: i64, min: i64, max: i64) -> i64 {
    query_param(url, key)
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}
