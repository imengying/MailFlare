//! 极简 HS256 JWT 签发与校验。
//!
//! 只做这一件事，因此不需要完整的 `jsonwebtoken`：
//! * 只有一个算法（HS256），不存在算法协商，天然免疫 `alg=none` /
//!   RS256→HS256 混淆类攻击；
//! * 没有 JWK、没有非对称密钥、没有随机数需求。
//!
//! 依赖只有 `hmac` + `sha2` + `base64`，三者都是纯 Rust，能直接编到 wasm。
//!
//! ## 为什么自己写而不是用 jsonwebtoken
//! `jsonwebtoken` 会拖进 `rsa` / `p256` / `p384` / `ed25519-dalek` 一整套非对称
//! 加密实现（只为支持我们根本不用的算法），并且内部锁定了 `getrandom ^0.2`
//! ——在 wasm32 上还得额外为它开 `js` feature。本模块把这些全部省掉。

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::util::constant_time_eq;

type HmacSha256 = Hmac<Sha256>;

/// HS256 的固定头部。`alg` 写死，校验时也只认这一个值。
const HEADER_JSON: &str = r#"{"alg":"HS256","typ":"JWT"}"#;

/// JWT 载荷。
///
/// `PartialEq` 只为测试里比较 `Result<Claims, _>` 服务。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// 主体：登录态固定为 `admin`；下载票据为该邮件 id。
    pub sub: String,
    /// 用途（scope）。
    pub scp: String,
    /// 签发时间（秒）。
    pub iat: i64,
    /// 过期时间（秒）。
    pub exp: i64,
}

/// 计算 HMAC-SHA256。
fn sign(secret: &[u8], message: &[u8]) -> Vec<u8> {
    // HMAC 接受任意长度密钥，`new_from_slice` 对它不会失败。
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(secret)
        .expect("HMAC 支持任意长度密钥");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// 签发 token：`base64url(header) + "." + base64url(claims) + "." + base64url(sig)`。
pub fn encode(secret: &[u8], claims: &Claims) -> Result<String, serde_json::Error> {
    let header = URL_SAFE_NO_PAD.encode(HEADER_JSON);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?);

    let mut signing_input = String::with_capacity(header.len() + payload.len() + 1);
    signing_input.push_str(&header);
    signing_input.push('.');
    signing_input.push_str(&payload);

    let signature = URL_SAFE_NO_PAD.encode(sign(secret, signing_input.as_bytes()));
    Ok(format!("{signing_input}.{signature}"))
}

/// 校验失败的原因。刻意不对外暴露细节，只用于内部区分。
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// 结构不是三段
    Malformed,
    /// base64 解码失败
    BadEncoding,
    /// 头部不是 HS256
    BadAlgorithm,
    /// 签名不匹配
    BadSignature,
    /// 载荷不是合法 JSON
    BadClaims,
    /// 已过期
    Expired,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Malformed => "token 结构错误",
            Self::BadEncoding => "token 编码错误",
            Self::BadAlgorithm => "不支持的签名算法",
            Self::BadSignature => "签名校验失败",
            Self::BadClaims => "token 载荷无效",
            Self::Expired => "token 已过期",
        };
        f.write_str(text)
    }
}

/// 校验 token 并返回载荷。
///
/// `now` 为当前时间（秒）；`leeway` 允许的时钟偏差。
/// 先验签名再解析载荷：签名不对时完全不碰攻击者控制的数据。
pub fn decode(
    secret: &[u8],
    token: &str,
    now: i64,
    leeway: i64,
) -> Result<Claims, VerifyError> {
    // 1. 切成三段。
    let mut parts = token.split('.');
    let (Some(header_b64), Some(payload_b64), Some(sig_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(VerifyError::Malformed);
    };

    // 2. 头部必须是 HS256 —— 只认这一个固定值，不做任何协商。
    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|_| VerifyError::BadEncoding)?;
    if header_bytes != HEADER_JSON.as_bytes() {
        return Err(VerifyError::BadAlgorithm);
    }

    // 3. 验签（常量时间比较，避免逐字节计时侧信道）。
    let expected = sign(secret, format!("{header_b64}.{payload_b64}").as_bytes());
    let actual = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| VerifyError::BadEncoding)?;
    if !constant_time_eq(&expected, &actual) {
        return Err(VerifyError::BadSignature);
    }

    // 4. 签名通过后才解析载荷。
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| VerifyError::BadEncoding)?;
    let claims: Claims =
        serde_json::from_slice(&payload_bytes).map_err(|_| VerifyError::BadClaims)?;

    // 5. 过期校验。
    if claims.exp + leeway < now {
        return Err(VerifyError::Expired);
    }

    Ok(claims)
}
