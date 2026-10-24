//! AWS Signature Version 4（R2 S3 API）。
//!
//! 纯函数，可在宿主机跑单测；有固定向量测试，算法写错会立刻暴露。
//! 只覆盖三个动词与固定区域 `auto`，不引入 `aws-sigv4` 那类依赖树。
//!
//! 签名写错只会得到 403，且不提示原因。三个易错点：URI 编码要保留 `/`、
//! 头部名要小写并按字典序排、派生密钥是四层 HMAC。

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// 空载荷的 SHA-256。`GET` / `HEAD` / `DELETE` 都签这个值。
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// 计算一个时刻对应的 `(YYYYMMDD, YYYYMMDDTHHMMSSZ)`（UTC）。
///
/// 自己算而不用 `chrono`：只需要这一种格式，多一个依赖不值得。
pub fn aws_timestamps(unix_ms: i64) -> (String, String) {
    let secs = unix_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let date = format!("{year:04}{month:02}{day:02}");
    let amz = format!("{date}T{hour:02}{minute:02}{second:02}Z");
    (date, amz)
}

/// 计算小写十六进制的 SHA-256。
pub fn sha256_hex(data: &[u8]) -> String {
    to_hex(&Sha256::digest(data))
}

/// 字节切片转小写十六进制。
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// 计算 HMAC-SHA256。
fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac =
        <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC 支持任意长度密钥");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// 按 SigV4 规则编码 URL 路径：除非保留字符与 `/` 外全部 `%HH` 大写。
///
/// R2 的 key 里只会出现十六进制 id 与数字，理论上不会触发编码；
/// 但仍然严格实现，避免将来改了 key 结构就在签名上踩坑。
pub fn encode_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if is_unreserved(byte) || byte == b'/' {
            out.push(byte as char);
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

/// `attr-char` / 非保留字符：`A-Z a-z 0-9 - _ . ~`。
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
}

/// 一次签名所需的输入。头部名称必须已经是小写。
pub struct SignRequest<'a> {
    pub method: &'a str,
    /// 已按 [`encode_uri_path`] 编码的路径（含桶名）。
    pub canonical_uri: &'a str,
    /// 需要参与签名的头部；必须包含 `host`、`x-amz-date`、
    /// `x-amz-content-sha256`。这里会按名称排序。
    pub headers: &'a [(&'a str, &'a str)],
    /// 载荷的 SHA-256（十六进制小写）。
    pub payload_hash: &'a str,
    pub access_key: &'a str,
    pub secret: &'a str,
    /// `YYYYMMDD`
    pub date: &'a str,
    /// `YYYYMMDDTHHMMSSZ`
    pub amz_date: &'a str,
}

/// 生成 `Authorization` 头的值。
pub fn authorization(req: &SignRequest<'_>) -> String {
    let mut headers: Vec<(&str, &str)> = req.headers.to_vec();
    headers.sort_by(|a, b| a.0.cmp(b.0));

    let mut canonical_headers = String::new();
    let mut signed = String::new();
    for (i, (name, value)) in headers.iter().enumerate() {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        // 值要去掉首尾空格；连续空格理论上还要折叠，但我们写入的头部
        // 都是自己构造的定长字符串，不存在这种情况。
        canonical_headers.push_str(value.trim());
        canonical_headers.push('\n');
        if i > 0 {
            signed.push(';');
        }
        signed.push_str(name);
    }

    // 查询串留空：我们的请求不带 query（Range 走头部，不参与签名）。
    let canonical_request = format!(
        "{method}\n{uri}\n\n{canonical_headers}\n{signed}\n{payload}",
        method = req.method,
        uri = req.canonical_uri,
        payload = req.payload_hash,
    );

    let scope = format!("{}/auto/s3/aws4_request", req.date);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz}\n{scope}\n{digest}",
        amz = req.amz_date,
        digest = sha256_hex(canonical_request.as_bytes()),
    );

    // 四层派生密钥。区域固定 `auto`，这是 R2 的要求。
    let k_date = hmac(format!("AWS4{}", req.secret).as_bytes(), req.date.as_bytes());
    let k_region = hmac(&k_date, b"auto");
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = to_hex(&hmac(&k_signing, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        req.access_key,
    )
}

/// Howard Hinnant 的 `civil_from_days`：把「距 1970-01-01 的天数」转成年月日。
///
/// 比自己处理闰年简单得多，也没有循环。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_at_known_instants() {
        assert_eq!(
            aws_timestamps(0),
            ("19700101".to_string(), "19700101T000000Z".to_string())
        );
        // 2025-01-01T00:00:00Z
        assert_eq!(
            aws_timestamps(1_735_689_600_000),
            ("20250101".to_string(), "20250101T000000Z".to_string())
        );
        // 闰日
        assert_eq!(
            aws_timestamps(1_709_164_800_000),
            ("20240229".to_string(), "20240229T000000Z".to_string())
        );
    }

    #[test]
    fn path_encoding_keeps_slashes() {
        assert_eq!(encode_uri_path("/bucket/mail/att/e1/0"), "/bucket/mail/att/e1/0");
        assert_eq!(encode_uri_path("/bucket/a b"), "/bucket/a%20b");
        // 非 ASCII 要编码成 UTF-8 的百分号形式。
        assert_eq!(encode_uri_path("/b/中"), "/b/%E4%B8%AD");
    }

    #[test]
    fn empty_payload_constant_is_correct() {
        assert_eq!(sha256_hex(b""), EMPTY_PAYLOAD_SHA256);
    }

    /// 固定向量：签名算法一旦写错，这个测试立刻失败。
    ///
    /// 期望值由 Python 标准库（`hashlib` + `hmac`）独立实现算出，
    /// 与本模块无共享代码，因此能真正起到交叉验证作用。
    /// 它同时锁住「派生密钥顺序 / 规范化请求格式 / 头部排序」三处易错点。
    #[test]
    fn signature_matches_independent_implementation() {
        let headers = [
            ("host", "acc.r2.cloudflarestorage.com"),
            ("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256),
            ("x-amz-date", "20250101T000000Z"),
        ];

        let sign = |method: &str| {
            authorization(&SignRequest {
                method,
                canonical_uri: "/bucket/mail/att/abc/0",
                headers: &headers,
                payload_hash: EMPTY_PAYLOAD_SHA256,
                access_key: "AKIDEXAMPLE",
                secret: "secret",
                date: "20250101",
                amz_date: "20250101T000000Z",
            })
        };

        // 每个动词的签名都与独立实现一致。
        assert_eq!(
            sign("GET").rsplit("Signature=").next().unwrap(),
            "6866e65cf28cf4e668450371fac053bf871535228896637d13483ce57a1b1ae4"
        );
        assert_eq!(
            sign("PUT").rsplit("Signature=").next().unwrap(),
            "79c476affb07de1d5c02e10619b56073f9b7a8b03e99cbf64e0fd9b4ce878018"
        );
        assert_eq!(
            sign("DELETE").rsplit("Signature=").next().unwrap(),
            "74ea315481064ba2bb4bb6b09892cdd7a1cd75d550856f3a1bfb60f09296b9a4"
        );

        // 顺带确认整体形态正确。
        let header = sign("GET");
        assert!(header.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20250101/auto/s3/aws4_request"
        ));
        assert!(header.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
    }

    #[test]
    fn header_order_does_not_matter() {
        // 传入顺序不同，签名结果必须一致（内部会排序）。
        let a = [
            ("host", "h"),
            ("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256),
            ("x-amz-date", "20250101T000000Z"),
        ];
        let b = [
            ("x-amz-date", "20250101T000000Z"),
            ("host", "h"),
            ("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256),
        ];
        let sign = |headers: &[(&str, &str)]| {
            authorization(&SignRequest {
                method: "GET",
                canonical_uri: "/b/k",
                headers,
                payload_hash: EMPTY_PAYLOAD_SHA256,
                access_key: "AK",
                secret: "s",
                date: "20250101",
                amz_date: "20250101T000000Z",
            })
        };
        assert_eq!(sign(&a), sign(&b));
    }
}
