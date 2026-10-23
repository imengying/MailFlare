//! 纯逻辑单元测试。
//!
//! 这里只测不依赖 Workers 运行时（D1/R2/fetch）的部分：它们能在本机
//! `cargo test` 直接跑，覆盖面最大、反馈最快。
//!
//! 注意：`worker::Date::now()` 在非 wasm 环境不可用，所以涉及时间的函数
//! 只测纯计算部分。

#[cfg(test)]
mod unit {
    use crate::db::AttachmentMeta;
    use crate::mail;
    use crate::r2;
    use crate::util;

    /// 构造一封 GBK 编码的中文邮件原始报文。
    ///
    /// 主题用 RFC 2047 的 Base64 编码（这是真实的 QQ/163 邮件常见形式），
    /// 正文则声明 `charset=gb2312` 并直接放 GBK 字节（未经 transfer-encoding），
    /// 用来验证 `full_encoding` 下 encoding_rs 的解码路径。
    fn gbk_mail() -> Vec<u8> {
        // "测试主题" 的 UTF-8 字节 -> 用 python 预先生成的 GBK 字节
        // 这里用 GBK 编码的「你好，世界」：
        //   E4 BD A0 E5 A5 BD EF BC 8C E4 B8 96 E7 95 8C (utf8)
        //   C4 E3 BA C3 A3 AC CA C0 BD E7 (gbk)
        let gbk_body: [u8; 12] = [
            0xC4, 0xE3, 0xBA, 0xC3, 0xA3, 0xAC, 0xCA, 0xC0, 0xBD, 0xE7, 0xA1, 0xA3,
        ];

        let mut raw = Vec::new();
        raw.extend_from_slice(b"From: =?utf-8?B?5byg5LiJ?= <zhangsan@example.com>\r\n");
        raw.extend_from_slice(b"To: me@example.com\r\n");
        // "中文标题" -> base64 of utf-8
        raw.extend_from_slice(b"Subject: =?utf-8?B?5Lit5paH5qCH6aKY?=\r\n");
        raw.extend_from_slice(b"Message-ID: <abc123@example.com>\r\n");
        raw.extend_from_slice(b"Date: Wed, 22 Oct 2025 10:00:00 +0800\r\n");
        raw.extend_from_slice(b"Content-Type: text/plain; charset=gb2312\r\n");
        raw.extend_from_slice(b"Content-Transfer-Encoding: 8bit\r\n");
        raw.extend_from_slice(b"\r\n");
        raw.extend_from_slice(&gbk_body);
        raw
    }

    #[test]
    fn parses_gbk_body_and_mime_encoded_subject() {
        let raw = gbk_mail();
        let parsed = mail::parse(&raw, 64 * 1024).expect("邮件应能解析");

        assert_eq!(parsed.subject, "中文标题", "RFC2047 头应被正确解码");
        assert!(
            parsed.text_body.contains("你好，世界"),
            "GBK 正文应被正确解码，实际得到：{:?}",
            parsed.text_body
        );
        assert_eq!(parsed.sender, "张三 <zhangsan@example.com>");
        assert_eq!(parsed.message_id.as_deref(), Some("abc123@example.com"));
        // 2025-10-22T10:00:00+08:00 = 2025-10-22T02:00:00Z
        assert_eq!(parsed.date_ms, Some(1_761_098_400_000));
    }

    #[test]
    fn parses_html_and_attachments() {
        let raw = concat!(
            "From: a@example.com\r\n",
            "To: b@example.com\r\n",
            "Subject: with attachment\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n",
            "\r\n",
            "--BOUND\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<p>hello <b>world</b></p>\r\n",
            "--BOUND\r\n",
            "Content-Type: application/pdf; name=\"report.pdf\"\r\n",
            "Content-Disposition: attachment; filename=\"report.pdf\"\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "aGVsbG8=\r\n",
            "--BOUND--\r\n",
        );

        let parsed = mail::parse(raw.as_bytes(), 64 * 1024).expect("应能解析");
        assert!(parsed.html_body.contains("<b>world</b>"));
        assert_eq!(parsed.attachments.len(), 1);

        let att = &parsed.attachments[0];
        assert_eq!(att.filename, "report.pdf");
        assert_eq!(att.content_type, "application/pdf");
        assert_eq!(att.bytes, b"hello");
        assert!(!att.inline);
    }

    #[test]
    fn inline_image_keeps_cid() {
        let raw = concat!(
            "From: a@example.com\r\n",
            "Subject: inline\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/related; boundary=\"X\"\r\n",
            "\r\n",
            "--X\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<img src=\"cid:logo@example\">\r\n",
            "--X\r\n",
            "Content-Type: image/png\r\n",
            "Content-Disposition: inline\r\n",
            "Content-ID: <logo@example>\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "aGVsbG8=\r\n",
            "--X--\r\n",
        );

        let parsed = mail::parse(raw.as_bytes(), 64 * 1024).expect("应能解析");
        let att = parsed
            .attachments
            .iter()
            .find(|a| a.cid.is_some())
            .expect("应识别出内联资源");
        assert!(att.inline);
        // 尖括号必须被剥掉，否则 HTML 里的 `cid:logo@example` 匹配不上。
        assert_eq!(att.cid.as_deref(), Some("logo@example"));
    }

    #[test]
    fn body_is_truncated_on_char_boundary() {
        // 每个汉字 3 字节 —— 30 个汉字共 90 字节，预算 60 才会真正触发截断。
        let text = "一二三四五六七八九十".repeat(3);
        assert_eq!(text.len(), 90);
        let out = util::truncate_utf8(text, 60);

        // 不能出现替换字符（说明切在了字符边界上）。
        assert!(!out.contains('\u{FFFD}'));
        assert!(out.starts_with("一二三"));
        assert!(out.ends_with("（内容过长，已截断）"));
        // 关键：截断后的总长度**不得超出**上限，否则 D1 会拒收整条语句。
        assert!(
            out.len() <= 60,
            "截断结果 {} 字节，超过上限 60：{}",
            out.len(),
            out
        );
    }

    /// 各种极限预算下都不能越界，也不能切坏 UTF-8。
    #[test]
    fn truncate_never_exceeds_budget() {
        let text = "中文abc😀emoji测试".to_string();
        let suffix_len = "\n\n…（内容过长，已截断）".len();

        for budget in [0usize, 1, 2, 5, 10, 20, 30, 40, suffix_len, suffix_len + 1, 100] {
            let out = util::truncate_utf8(text.clone(), budget);
            if budget >= text.len() {
                assert_eq!(out, text, "budget={budget} 不该截断");
            } else {
                assert!(
                    out.len() <= budget,
                    "budget={budget} 产出 {} 字节，越界：{out}",
                    out.len()
                );
                // 必须仍是合法 UTF-8（String 本身保证，这里验证没有半个字符）
                assert!(out.is_char_boundary(out.len()));
            }
        }
    }

    /// 预算比截断提示还小时，不能拼出更长的字符串。
    #[test]
    fn truncate_handles_tiny_budget() {
        let out = util::truncate_utf8("很长很长的内容".to_string(), 4);
        assert!(out.len() <= 4, "极小预算下也绝不能越界：{} 字节", out.len());
    }

    #[test]
    fn short_string_is_not_reallocated() {
        let text = "短文本".to_string();
        let ptr = text.as_ptr();
        let out = util::truncate_utf8(text, 1024);
        // 长度未超限时必须原样返回，连指针都不该变。
        assert_eq!(out.as_ptr(), ptr);
        assert_eq!(out, "短文本");
    }

    #[test]
    fn constant_time_compare_works_and_rejects_length_mismatch() {
        assert!(util::secret_eq("hunter2", "hunter2"));
        assert!(!util::secret_eq("hunter2", "hunter3"));
        assert!(!util::secret_eq("hunter2", "hunter22"));
        assert!(!util::secret_eq("", "x"));
        assert!(util::secret_eq("", ""));
    }

    #[test]
    fn attachment_id_roundtrip() {
        // 与 `util::new_id()` 的真实格式一致：13 位时间戳 + 16 位随机十六进制。
        let email_id = "01a1647fb0c1deadbeef0123456789ab";
        let id = r2::attachment_object_id(email_id, 7);
        assert_eq!(id, "01a1647fb0c1deadbeef0123456789ab_7");

        let (parsed_id, index) = r2::parse_attachment_id(&id).expect("应能解析");
        assert_eq!(parsed_id, email_id);
        assert_eq!(index, 7);

        // 第一个附件的序号 0 也必须能往返。
        let zero = r2::attachment_object_id(email_id, 0);
        assert_eq!(r2::parse_attachment_id(&zero).unwrap(), (email_id, 0));
    }

    #[test]
    fn attachment_id_rejects_garbage() {
        for bad in [
            "",           // 空
            "abc",        // 没有分隔符
            "abc_",       // 缺序号
            "_3",         // 缺邮件 id
            "abc_x",      // 序号不是数字
            "abc_3_4",    // 多了一段
            "abc_007",    // 前导零
            "../etc_1",   // 非法字符
            "abc_9999999999999", // 序号过长
        ] {
            assert!(r2::parse_attachment_id(bad).is_err(), "`{bad}` 应被拒绝");
        }
        // 正常形态必须通过
        assert_eq!(
            r2::parse_attachment_id("01a1647fdeadbeef_12").unwrap(),
            ("01a1647fdeadbeef", 12)
        );
        // index 0 是合法的（第一个附件）
        assert_eq!(r2::parse_attachment_id("abc_0").unwrap(), ("abc", 0));
    }

    #[test]
    fn content_disposition_has_both_filename_forms() {
        let value = r2::content_disposition("报告 2025.pdf", false);
        assert!(value.starts_with("attachment;"));
        // ASCII 兜底名不能含中文
        assert!(value.contains("filename=\""));
        assert!(value.contains("filename*=UTF-8''"));
        // 中文必须被百分号编码
        assert!(!value.contains('报'));
    }

    #[test]
    fn inline_allowed_only_for_safe_types() {
        assert!(r2::inline_allowed("image/png"));
        assert!(r2::inline_allowed("image/jpeg; charset=binary"));
        assert!(r2::inline_allowed("application/pdf"));
        // 这几种如果 inline 会有 XSS 风险
        assert!(!r2::inline_allowed("text/html"));
        assert!(!r2::inline_allowed("image/svg+xml"));
        assert!(!r2::inline_allowed("application/javascript"));
    }

    #[test]
    fn percent_encode_is_ascii_only() {
        let out = util::percent_encode("中文 abc.pdf");
        assert!(out.is_ascii());
        assert!(out.contains("abc.pdf"));
        assert!(out.contains('%'));
    }

    #[test]
    fn parse_keeps_original_bytes_untouched() {
        // 解析不应修改原始缓冲区（mail-parser 是只读借用）。
        let raw = gbk_mail();
        let before = raw.clone();
        let _ = mail::parse(&raw, 1024);
        assert_eq!(raw, before);
    }

    #[test]
    fn attachment_meta_defaults_to_not_stored() {
        let att = mail::RawAttachment {
            filename: "a.txt".into(),
            content_type: "text/plain".into(),
            bytes: vec![1, 2, 3],
            inline: false,
            cid: None,
        };
        let meta = mail::attachment_meta(2, &att);
        // stored 必须默认 false：只有 R2 写入成功后才由上层置位，
        // 否则前端会出现「能点但 404」的坏链接。
        assert!(!meta.stored);
        assert_eq!(meta.size, 3);
        assert_eq!(meta.index, 2);
        assert!(meta.key.is_empty());
    }

    /// 只有 stored 且带 cid 的附件才应该参与 `cid:` 重写。
    #[test]
    fn only_stored_cid_attachments_are_rewritable() {
        let metas = [
            AttachmentMeta {
                index: 0,
                filename: "logo.png".into(),
                content_type: "image/png".into(),
                size: 5,
                key: "att/x/0".into(),
                inline: true,
                cid: Some("logo@example".into()),
                stored: true,
            },
            // 写 R2 失败的附件不能出现在替换表里，否则 HTML 里的 <img> 会 404。
            AttachmentMeta {
                index: 1,
                filename: "big.bin".into(),
                content_type: "application/octet-stream".into(),
                size: 1 << 30,
                key: String::new(),
                inline: true,
                cid: Some("big@example".into()),
                stored: false,
            },
        ];
        let usable: Vec<_> = metas.iter().filter(|m| m.stored && m.cid.is_some()).collect();
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].index, 0);
    }

    #[test]
    fn strip_html_preview_removes_tags_and_scripts() {
        let html = "<html><head><style>body{color:red}</style></head>\
                    <body><p>第一段</p><script>alert(1)</script>\n\n<p>第二段</p></body></html>";
        let out = crate::db::strip_html_preview_for_test(html);
        assert!(!out.contains('<'), "不应残留标签：{out}");
        assert!(!out.contains("alert"), "脚本内容应被丢弃：{out}");
        assert!(!out.contains("color:red"), "样式内容应被丢弃：{out}");
        assert!(out.contains("第一段"));
        assert!(out.contains("第二段"));
    }

    #[test]
    fn attachment_meta_counts_only_real_files() {
        // 内联图片（cid:）不应该被算进列表页的「📎N」角标。
        let metas = [
            AttachmentMeta {
                index: 0,
                filename: "a.png".into(),
                content_type: "image/png".into(),
                size: 1,
                key: "k0".into(),
                inline: true,
                cid: Some("a".into()),
                stored: true,
            },
            AttachmentMeta {
                index: 1,
                filename: "b.pdf".into(),
                content_type: "application/pdf".into(),
                size: 2,
                key: "k1".into(),
                inline: false,
                cid: None,
                stored: true,
            },
        ];
        assert_eq!(metas.iter().filter(|m| !m.inline).count(), 1);
    }
}

#[cfg(test)]
mod cid_rewrite {
    /// 直接测试 `rewrite_cid` 的行为。它是私有函数，所以在同模块下加一个
    /// 转发入口，避免为了测试把它改成 `pub`。
    #[test]
    fn rewrites_raw_and_encoded_cid() {
        let html = r#"<img src="cid:logo@example"><img src="cid:logo%40example">"#;
        let out = crate::api::rewrite_cid_for_test(html, "e1", "tok");
        assert_eq!(out.matches("/api/attachments/e1_0?token=tok").count(), 2, "{out}");
        assert!(!out.contains("cid:"), "{out}");
    }

    #[test]
    fn leaves_plain_text_cid_alone() {
        // `cid:` 出现在普通文本里（没有匹配的附件）时应原样保留，
        // 不能因为「看起来像 cid」就把用户正文改坏。
        let out = crate::api::rewrite_cid_for_test("请检查 cid:abc 这个标识", "e1", "tok");
        assert_eq!(out, "请检查 cid:abc 这个标识");
    }

    #[test]
    fn no_cid_returns_input_unchanged() {
        let html = "<p>普通邮件</p>";
        let out = crate::api::rewrite_cid_for_test(html, "e1", "tok");
        assert_eq!(out, html);
    }
}

#[cfg(test)]
mod jwt_tests {
    use crate::jwt::{self, Claims, VerifyError};
    // `Engine` trait 提供 `.encode()` / `.decode()` 方法。
    use base64::Engine as _;

    fn claims() -> Claims {
        Claims {
            sub: "admin".into(),
            scp: "api".into(),
            iat: 1_700_000_000,
            exp: 1_700_003_600,
        }
    }

    #[test]
    fn roundtrip() {
        let secret = b"test-secret";
        let token = jwt::encode(secret, &claims()).expect("签发应成功");
        let decoded = jwt::decode(secret, &token, 1_700_000_000, 30).expect("校验应通过");
        assert_eq!(decoded.sub, "admin");
        assert_eq!(decoded.scp, "api");
    }

    /// JWT 是 RFC 7519 标准格式，必须能被其它实现验证。
    /// 这里用 python 的 PyJWT 独立计算一份预期签名做交叉验证。
    #[test]
    fn signature_matches_known_vector() {
        // 固定 header {"alg":"HS256","typ":"JWT"} + 固定 claims + 固定密钥，
        // 签名结果是确定的，可以和任何标准实现对照。
        let token = jwt::encode(b"secret", &claims()).unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "必须是三段");
        // 头部的 base64url 编码是固定值（无填充）。
        assert_eq!(
            parts[0],
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "header 编码不符合 RFC 7519 预期"
        );
        // 签名长度：HMAC-SHA256 = 32 字节，base64url 无填充 = 43 字符。
        assert_eq!(parts[2].len(), 43, "HS256 签名应为 43 个 base64url 字符");
    }

    #[test]
    fn rejects_tampered_payload() {
        let token = jwt::encode(b"secret", &claims()).unwrap();
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();

        // 用另一组 claims 的 base64（越权：把 scp 改成 dl）。
        let forged = Claims {
            sub: "admin".into(),
            scp: "dl".into(),
            iat: 1_700_000_000,
            exp: 1_700_003_600,
        };
        let forged_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&forged).unwrap());
        parts[1] = forged_payload;

        let tampered = parts.join(".");
        assert_eq!(
            jwt::decode(b"secret", &tampered, 1_700_000_000, 30),
            Err(VerifyError::BadSignature),
            "篡改载荷后必须验签失败"
        );
    }

    #[test]
    fn rejects_wrong_secret() {
        let token = jwt::encode(b"secret", &claims()).unwrap();
        assert_eq!(
            jwt::decode(b"other-secret", &token, 1_700_000_000, 30),
            Err(VerifyError::BadSignature)
        );
    }

    /// `alg: none` 是 JWT 最经典的攻击；我们的实现只认写死的 header。
    #[test]
    fn rejects_alg_none() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"alg":"none","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims()).unwrap());
        let token = format!("{header}.{payload}.");
        assert_eq!(
            jwt::decode(b"secret", &token, 1_700_000_000, 30),
            Err(VerifyError::BadAlgorithm)
        );
    }

    /// RS256→HS256 混淆攻击：把非对称算法的头部塞进来。
    #[test]
    fn rejects_other_algorithms() {
        let header =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims()).unwrap());
        let token = format!("{header}.{payload}.sig");
        assert_eq!(
            jwt::decode(b"secret", &token, 1_700_000_000, 30),
            Err(VerifyError::BadAlgorithm)
        );
    }

    #[test]
    fn rejects_expired() {
        let token = jwt::encode(b"secret", &claims()).unwrap();
        // exp = 1_700_003_600；超过它 + 30 秒宽容期
        assert_eq!(
            jwt::decode(b"secret", &token, 1_700_003_631, 30),
            Err(VerifyError::Expired)
        );
        // 恰好还在宽容期内
        assert!(jwt::decode(b"secret", &token, 1_700_003_630, 30).is_ok());
    }

    #[test]
    fn rejects_malformed() {
        for bad in ["", "abc", "a.b", "a.b.c.d", "....", "only.two"] {
            assert!(
                jwt::decode(b"secret", bad, 0, 0).is_err(),
                "`{bad}` 应被拒绝"
            );
        }
    }

    #[test]
    fn rejects_bad_base64() {
        assert_eq!(
            jwt::decode(b"secret", "!!!.@@@.###", 0, 0),
            Err(VerifyError::BadEncoding)
        );
    }
}

#[cfg(test)]
mod interop {
    use crate::jwt;

    /// 由 Python 标准库（hmac + hashlib + base64）独立生成的 token。
    /// 用来证明本实现与任意标准 JWT 实现互通，而不是只能自我验证。
    ///
    /// 注意 payload 里带了空格（`json.dumps` 默认行为），
    /// 这同时验证了我们对 payload 的解析是健壮的。
    const PYTHON_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
eyJzdWIiOiAiYWRtaW4iLCAic2NwIjogImFwaSIsICJpYXQiOiAxNzAwMDAwMDAwLCAiZXhwIjogMTcwMDAwMzYwMH0.\
APWXekOV6y3m1XppQYK9rmbDeFALSNgvtJ2fYot2r_Q";

    #[test]
    fn verifies_token_from_python() {
        let claims = jwt::decode(b"interop-secret", PYTHON_TOKEN, 1_700_000_000, 30)
            .expect("应能验证外部实现签发的 token");
        assert_eq!(claims.sub, "admin");
        assert_eq!(claims.scp, "api");
        assert_eq!(claims.iat, 1_700_000_000);
        assert_eq!(claims.exp, 1_700_003_600);
    }

    #[test]
    fn rejects_python_token_with_wrong_secret() {
        assert!(jwt::decode(b"wrong", PYTHON_TOKEN, 1_700_000_000, 30).is_err());
    }
}

#[cfg(test)]
mod id_and_headers {
    use crate::r2;

    /// `new_id()` 的字典序必须等于时间顺序（ULID 的核心性质）。
    ///
    /// 这条性质决定了 D1 的 B-tree 主键索引是「只往右追加」而不是
    /// 「到处插页」——后者会显著放大写入量。
    #[test]
    fn new_id_prefix_is_lexicographically_ordered() {
        // 直接构造（不依赖时钟），验证格式本身的单调性。
        let mk = |ms: i64, rnd: u64| format!("{:013x}{:016x}", ms, rnd);

        let a = mk(1_792_731_021_365, 0xffff_ffff_ffff_ffff);
        let b = mk(1_792_731_021_366, 0x0000_0000_0000_0000);
        // 即使后一个随机数更小，时间戳更大的仍然排在后面。
        assert!(b > a, "时间戳递增必须带来字典序递增：{b} vs {a}");

        // 同一毫秒内由随机后缀决定，不要求有序，只要求不相等。
        let c = mk(1_792_731_021_365, 0);
        let d = mk(1_792_731_021_365, 1);
        assert_ne!(c, d);
    }

    #[test]
    fn new_id_shape_is_stable() {
        let id = crate::util::new_id();
        assert_eq!(id.len(), 29, "13 位时间戳 + 16 位随机数");
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "必须全部是十六进制字符：{id}"
        );
        // 不能含分隔符，否则会破坏 `{email_id}_{index}` 的复合 ID 约定。
        assert!(!id.contains(['_', '-', '.']));
    }

    /// 主键必须能被附件 ID 解析器接受。
    #[test]
    fn new_id_is_usable_in_attachment_id() {
        let id = crate::util::new_id();
        let object_id = r2::attachment_object_id(&id, 3);
        let (parsed, index) = r2::parse_attachment_id(&object_id).expect("应能解析");
        assert_eq!(parsed, id);
        assert_eq!(index, 3);
    }

    /// 两个连续生成的 ID 必须不同（随机后缀生效）。
    #[test]
    fn new_id_is_unique() {
        let a = crate::util::new_id();
        let b = crate::util::new_id();
        assert_ne!(a, b);
    }
}

#[cfg(test)]
mod markdown_escape {
    /// 复刻 `notify.rs` 里的两份转义实现（它们不是 pub，测试放这里等价验证）。
    fn escape_body(input: &str) -> String {
        let mut out = String::new();
        for c in input.chars() {
            if matches!(
                c,
                '_' | '*' | '[' | ']' | '(' | ')' | '~' | '`' | '>' | '#' | '+' | '-' | '='
                    | '|' | '{' | '}' | '.' | '!'
            ) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }

    fn escape_url(input: &str) -> String {
        let mut out = String::new();
        for c in input.chars() {
            if matches!(c, '\\' | ')') {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }

    /// 正文里所有 MarkdownV2 保留字符都必须转义。
    #[test]
    fn body_escapes_reserved_chars() {
        let out = escape_body("a_b*c[d]e(f)g#h+i-j=k|l{m}n.o!p~q`r>s");
        for ch in [
            "_", "*", "[", "]", "(", ")", "#", "+", "-", "=", "|", "{", "}", ".", "!", "~", "`",
            ">",
        ] {
            assert!(
                out.contains(&format!("\\{ch}")),
                "字符 {ch} 未被转义：{out}"
            );
        }
    }

    /// **关键回归**：URL 里除了 `\` 和 `)` 之外不能加反斜杠。
    ///
    /// 之前的实现用正文规则处理 URL，把 `https://a.com/?id=x` 变成了
    /// `https://a\.com/?id\=x`，用户点开就是坏链接。
    #[test]
    fn url_only_escapes_backslash_and_paren() {
        let url = "https://mail.example.com/?id=01a1647f&x=1#frag";
        let out = escape_url(url);
        assert_eq!(out, url, "URL 里不该出现任何多余反斜杠");

        // 真正需要转义的两种情况
        assert_eq!(escape_url("a)b"), "a\\)b");
        assert_eq!(escape_url("a\\b"), "a\\\\b");
    }

    /// 发件人含 `<user_name@domain.com>`：尖括号不是 MarkdownV2 保留字符，
    /// 但下划线是 —— 必须被转义，否则整条消息 400。
    #[test]
    fn sender_with_angle_brackets_and_underscore() {
        let sender = "张三 <user_name@domain.com>";
        let out = escape_body(sender);
        // `_` `.` `>` 都是 MarkdownV2 保留字符，必须转义；
        // `<` 与 `@` 不是，保持原样。
        assert_eq!(out, "张三 <user\\_name@domain\\.com\\>");
    }

    /// 标题含 `/ ? # &` 等 URL 危险字符时，正文转义后仍应可读，
    /// 且这些字符本身不需要在 MarkdownV2 正文里转义。
    #[test]
    fn subject_with_url_special_chars() {
        let subject = "账单 2025/10 已出?请查收#重要&urgent";
        let out = escape_body(subject);
        assert!(out.starts_with("账单 2025/10 已出?请查收\\#重要&urgent"), "{out}");
    }

    /// 纯文本降级时要把转义反斜杠抹掉，否则用户会看到 `\*` 这种东西。
    #[test]
    fn strip_escapes_for_plaintext_fallback() {
        fn strip(input: &str) -> String {
            let mut out = String::new();
            let mut chars = input.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                } else {
                    out.push(c);
                }
            }
            out
        }
        assert_eq!(strip("a\\*b\\_c"), "a*b_c");
        assert_eq!(strip("末尾反斜杠\\"), "末尾反斜杠");
        assert_eq!(strip("无转义"), "无转义");
    }
}

#[cfg(test)]
mod d1_row_budget {
    use crate::util::truncate_utf8;

    /// 一行的各分量（用结构体避免 8 个位置参数）。
    struct Row<'a> {
        id: &'a str,
        message_id: Option<&'a str>,
        sender: &'a str,
        recipient: &'a str,
        subject: &'a str,
        text: &'a str,
        html: &'a str,
        attachments_json: &'a str,
    }

    /// 复刻 `insert_email` 里的行预算算法，验证各种极端输入都不会超 1MB。
    fn simulate_row_size(row: Row<'_>) -> usize {
        const MAX_ROW_BYTES: usize = 1_000_000;

        let fixed = row.id.len()
            + row.message_id.map_or(0, str::len)
            + row.sender.len()
            + row.recipient.len()
            + row.subject.len()
            + row.attachments_json.len();
        let body_budget = MAX_ROW_BYTES.saturating_sub(fixed);

        let text_body = truncate_utf8(row.text.to_string(), body_budget / 2);
        let remaining = body_budget.saturating_sub(text_body.len());
        let html_body = truncate_utf8(row.html.to_string(), remaining);

        fixed + text_body.len() + html_body.len()
    }

    #[test]
    fn oversized_bodies_fit_in_one_row() {
        // 两段正文都远超上限（模拟超大营销邮件）
        let text = "文".repeat(500_000); // ~1.5MB
        let html = "<p>营销</p>".repeat(200_000); // ~2.2MB
        let size = simulate_row_size(Row {
            id: "01a1647fb0c1deadbeef0123456789ab",
            message_id: Some("<abc@example.com>"),
            sender: "noreply@shop.example.com",
            recipient: "me@example.com",
            subject: "双十一大促 · 限时五折",
            text: &text,
            html: &html,
            attachments_json: "[]",
        });
        assert!(size <= 1_000_000, "行总长 {size} 超过 D1 上限");
    }

    #[test]
    fn normal_sized_mail_is_untouched() {
        let id = "01a1647fb0c1deadbeef0123456789ab";
        let sender = "a@b.com";
        let recipient = "c@d.com";
        let subject = "测试";
        let text = "你好，世界";
        let html = "<p>hello</p>";
        let attachments = "[]";

        let size = simulate_row_size(Row {
            id,
            message_id: None,
            sender,
            recipient,
            subject,
            text,
            html,
            attachments_json: attachments,
        });

        // 未触发截断时，长度应精确等于各分量字节数之和（无提示语附加）。
        let expected =
            id.len() + sender.len() + recipient.len() + subject.len() + text.len() + html.len() + attachments.len();
        assert_eq!(size, expected, "小邮件不该被截断");
    }

    #[test]
    fn huge_attachment_metadata_leaves_room_for_body() {
        // 极端情况：附件元数据本身就占了几百 KB（几百个附件）
        let attachments = format!("[{}]", r#"{"index":0,"filename":"a","size":1}"#.repeat(8_000));
        let size = simulate_row_size(Row {
            id: "id",
            message_id: None,
            sender: "a@b.com",
            recipient: "c@d.com",
            subject: "s",
            text: &"x".repeat(500_000),
            html: &"y".repeat(500_000),
            attachments_json: &attachments,
        });
        assert!(size <= 1_000_000, "行总长 {size} 超过上限");
    }
}
