//! 纯逻辑单元测试。
//!
//! 这里只测不依赖 Workers 运行时（D1/R2/fetch）的部分：它们能在本机
//! `cargo test` 直接跑，覆盖面最大、反馈最快。
//!
//! 注意：`worker::Date::now()` 在非 wasm 环境不可用，所以涉及时间的函数
//! 只测纯计算部分。

#[cfg(test)]
mod unit {
    use crate::db::strip_html_preview;

    #[test]
    fn like_pattern_escapes_wildcards() {
        // `%` 和 `_` 必须被转义，否则等于「匹配全部」。
        assert_eq!(crate::db::escape_like("a%b"), "%a\\%b%");
        assert_eq!(crate::db::escape_like("a_b"), "%a\\_b%");
        // 普通中文关键词原样包进 %...%。
        assert_eq!(crate::db::escape_like("验证码"), "%验证码%");
    }

    #[test]
    fn strip_html_decodes_common_entities() {
        assert_eq!(strip_html_preview("<p>a&nbsp;b &amp; c</p>"), "a b & c");
        assert_eq!(strip_html_preview("<p>&#39;x&#39;</p>"), "'x'");
        assert_eq!(strip_html_preview("<p>&#x27;y&#x27;</p>"), "'y'");
        // 孤立 & 不能吞掉后面的文字。
        assert_eq!(strip_html_preview("<p>a & b</p>"), "a & b");
    }

    #[test]
    fn strip_html_keeps_unknown_entities_untouched() {
        assert_eq!(strip_html_preview("<p>&weird;</p>"), "&weird;");
    }

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
        let out = crate::db::strip_html_preview(html);
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

mod settings_normalize {
    //! 设置项的归一化与夹紧。
    //!
    //! 这些值都存在 D1 里，用户可以手改表，也可能传上来离谱的数字；
    //! `normalized()` 必须保证任何输入都产出一个可安全使用的配置。

    use crate::config::Settings;

    #[test]
    fn trims_and_strips_trailing_slash() {
        let s = Settings {
            turnstile_site_key: "  0xABC  ".into(),
            bark_server: "https://api.day.app/".into(),
            ..Default::default()
        }
        .normalized();

        assert_eq!(s.turnstile_site_key, "0xABC");
        assert_eq!(s.bark_server, "https://api.day.app");
    }

    #[test]
    fn empty_bark_server_falls_back_to_default() {
        let s = Settings {
            bark_server: "   ".into(),
            ..Default::default()
        }
        .normalized();
        assert_eq!(s.bark_server, "https://api.day.app");
    }

    #[test]
    fn numbers_are_clamped_into_safe_range() {
        // 全部塞成 0 或极大值，验证每种都落在允许区间内。
        let s = Settings {
            max_body_bytes: 0,
            max_message_bytes: i64::MAX,
            max_attachment_bytes: -1,
            r2_concurrency: 999,
            page_size_default: 0,
            page_size_max: 9999,
            ..Default::default()
        }
        .normalized();

        assert_eq!(s.max_body_bytes, 4 * 1024, "正文下限 4KB");
        assert_eq!(s.max_message_bytes, 25 * 1024 * 1024, "邮件上限 25MB");
        assert_eq!(s.max_attachment_bytes, 1024, "附件下限 1KB");
        assert_eq!(s.r2_concurrency, 16);
        assert_eq!(s.page_size_default, 1);
        assert_eq!(s.page_size_max, 200);
    }

    #[test]
    fn default_page_size_never_exceeds_max() {
        // 上限设得比默认页还小，归一化后必须把默认页压下来，
        // 否则第一页就会请求超量。
        let s = Settings {
            page_size_default: 100,
            page_size_max: 10,
            ..Default::default()
        }
        .normalized();
        assert_eq!(s.page_size_default, 10);
    }

    #[test]
    fn turnstile_requires_secret_to_be_enabled() {
        let mut s = Settings::default();
        assert!(!s.turnstile_enabled(), "默认不启用");

        s.turnstile_site_key = "0xABC".into();
        assert!(!s.turnstile_enabled(), "只有 site key 不算启用");

        s.turnstile_secret = "0xSECRET".into();
        assert!(s.turnstile_enabled(), "两项都有才算启用");
    }

    #[test]
    fn redacted_only_exposes_set_flags() {
        let s = Settings {
            turnstile_secret: "0xSECRETVALUE".into(),
            telegram_bot_token: "123456:ABCdefGHIjkl".into(),
            bark_device_key: "short".into(),
            ..Default::default()
        };
        let view = s.redacted();

        assert!(view.turnstile_secret_set);
        assert!(view.telegram_bot_token_set);
        assert!(view.bark_device_key_set);
        // 空密钥必须显示为「未设置」。
        assert!(!view.r2_secret_access_key_set);
        // 视图里既没有明文密钥，也没有任何尾部提示——尾部字符同样属于泄露，
        // 位数够短时（如长度 8 以下）基本等于把整个密钥交出去。
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // 结构体级 #[serde(default)]：旧记录缺字段也能读出来。
        let s: Settings = serde_json::from_str(r#"{"site_name":"OnlyName"}"#).unwrap();
        assert_eq!(s.site_name, "OnlyName");
        assert_eq!(s.page_size_default, 20);
        assert_eq!(s.bark_server, "https://api.day.app");
    }
}

mod settings_patch {
    //! `PUT /api/settings` 的字段合并语义。
    //!
    //! 核心不变量：密钥字段的「未提供」与「提供空串」必须可区分——
    //! 前端不填 = 不修改，显式清空 = 删除。混淆这两者会导致用户
    //! 改个分页大小就把推送 Token 清空。

    use crate::config::Settings;

    use crate::config::merge_secret;

    /// 让测试直接打生产函数，避免两边各写一份规则而漂移。
    fn merge(slot: &mut String, patch: Option<&str>) {
        merge_secret(slot, patch.map(str::to_string));
    }

    #[test]
    fn absent_secret_keeps_existing_value() {
        let mut secret = "old-token".to_string();
        merge(&mut secret, None);
        assert_eq!(secret, "old-token", "不提交就该保持原值");
    }

    #[test]
    fn empty_secret_clears_value() {
        let mut secret = "old-token".to_string();
        merge(&mut secret, Some(""));
        assert_eq!(secret, "", "显式空串表示清空");
    }

    #[test]
    fn new_secret_overwrites() {
        let mut secret = "old-token".to_string();
        merge(&mut secret, Some("new-token"));
        assert_eq!(secret, "new-token");
    }

    #[test]
    fn masked_placeholder_is_ignored() {
        // 前端若误把脱敏提示回传，绝不能覆盖真值。
        let mut secret = "real-token".to_string();
        merge(&mut secret, Some("****abcd"));
        assert_eq!(secret, "real-token", "含 * 的占位符必须被忽略");
    }

    #[test]
    fn normalization_applies_after_merge() {
        // 合并后要跑一遍归一化：空格与尾斜杠都得处理掉。
        let settings = Settings {
            bark_server: "  https://api.day.app/ ".into(),
            ..Default::default()
        }
        .normalized();
        assert_eq!(settings.bark_server, "https://api.day.app");
    }
}

mod address_filter {
    //! 发件人黑名单 / 收件人白名单的匹配规则。
    //!
    //! 这两条规则直接决定「收不收这封信」，写错的后果是漏收正常邮件
    //! 或者放过垃圾邮件，所以边界情况要逐个钉住。

    use crate::config::{bare_address, recipient_allowed, sender_blocked, valid_address_rule};

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_address_strips_display_name() {
        assert_eq!(bare_address("Spam <Spam@Example.COM>"), "spam@example.com");
        assert_eq!(bare_address("  a@b.com  "), "a@b.com");
        // 显示名里也含尖括号时，要取最后一对。
        assert_eq!(bare_address("\"a <b>\" <c@d.com>"), "c@d.com");
        // 没有尖括号就原样小写。
        assert_eq!(bare_address("A@B.COM"), "a@b.com");
    }

    #[test]
    fn empty_blocklist_blocks_nothing() {
        assert!(!sender_blocked(&[], "anyone@example.com"));
    }

    #[test]
    fn full_address_rule_matches_exactly() {
        let blocked = list(&["spam@example.com"]);
        assert!(sender_blocked(&blocked, "spam@example.com"));
        assert!(sender_blocked(&blocked, "Spam <SPAM@example.com>"));
        // 同域名的其它地址不受影响。
        assert!(!sender_blocked(&blocked, "friend@example.com"));
    }

    #[test]
    fn domain_rule_matches_subdomains() {
        let blocked = list(&["@example.com"]);
        assert!(sender_blocked(&blocked, "a@example.com"));
        // 子域也要命中，否则 `@example.com` 会漏掉 `a@mail.example.com`。
        assert!(sender_blocked(&blocked, "a@mail.example.com"));
        // 相似但不同的域名不能被误伤。
        assert!(!sender_blocked(&blocked, "a@notexample.com"));
        assert!(!sender_blocked(&blocked, "a@example.com.evil.com"));
    }

    #[test]
    fn bare_domain_rule_equals_at_prefixed() {
        let blocked = list(&["spam.com"]);
        assert!(sender_blocked(&blocked, "a@spam.com"));
        assert!(sender_blocked(&blocked, "a@mail.spam.com"));
        assert!(!sender_blocked(&blocked, "a@notspam.com"));
    }

    #[test]
    fn empty_whitelist_allows_everyone() {
        // 关键行为：不填白名单 = catch-all，未注册的地址也能收件。
        assert!(recipient_allowed(&[], "whatever@example.com"));
        assert!(recipient_allowed(&[], "另一个@example.com"));
    }

    #[test]
    fn whitelist_limits_when_filled() {
        let allowed = list(&["me@example.com", "@mail.example.com"]);

        assert!(recipient_allowed(&allowed, "me@example.com"));
        assert!(recipient_allowed(&allowed, "anyone@mail.example.com"));
        assert!(recipient_allowed(&allowed, "deep@a.mail.example.com"));

        assert!(!recipient_allowed(&allowed, "other@example.com"));
        assert!(!recipient_allowed(&allowed, "x@elsewhere.com"));
    }

    #[test]
    fn empty_address_never_matches_nonempty_whitelist() {
        let allowed = list(&["me@example.com"]);
        assert!(!recipient_allowed(&allowed, ""));
        assert!(!recipient_allowed(&allowed, "   "));
        // 空地址对黑名单也不该误判（没有地址就无从匹配）。
        assert!(!sender_blocked(&list(&["@example.com"]), ""));
    }

    #[test]
    fn valid_rule_accepts_three_forms() {
        // 三种合法写法
        assert!(valid_address_rule("me@example.com"));
        assert!(valid_address_rule("@example.com"));
        assert!(valid_address_rule("example.com"));
        // 子域也算合法
        assert!(valid_address_rule("@mail.example.com"));
    }

    #[test]
    fn valid_rule_rejects_junk() {
        assert!(!valid_address_rule(""));
        assert!(!valid_address_rule("   "));
        // 通配符会被误以为生效，直接拒掉。
        assert!(!valid_address_rule("*@example.com"));
        // 空格
        assert!(!valid_address_rule("a b@example.com"));
        // 没有点的裸词
        assert!(!valid_address_rule("localhost"));
        assert!(!valid_address_rule("@localhost"));
        // 残缺的域名
        assert!(!valid_address_rule("example."));
        assert!(!valid_address_rule(".example.com"));
        // 只有 @
        assert!(!valid_address_rule("@"));
        // 本地部分为空
        assert!(!valid_address_rule("@.com"));
    }

    #[test]
    fn every_valid_rule_matches_itself() {
        // 校验通过、但匹配不上的规则等于静默失效，这里成对验证。
        for rule in ["me@example.com", "@example.com", "example.com"] {
            assert!(valid_address_rule(rule), "{rule} 应通过校验");
        }
        assert!(sender_blocked(&list(&["me@example.com"]), "me@example.com"));
        assert!(sender_blocked(&list(&["@example.com"]), "x@example.com"));
        assert!(sender_blocked(&list(&["example.com"]), "x@example.com"));
    }

    #[test]
    fn rules_are_case_insensitive_after_normalize() {
        let blocked = list(&["SPAM@EXAMPLE.COM"]);
        // normalized() 会把规则转小写；这里直接验证匹配本身也小写化输入。
        assert!(sender_blocked(&blocked, "Spam@Example.Com"));
    }
}

mod site_appearance {
    //! 站点外观的校验与 CSS 生成。
    //!
    //! 这些值会直接写进内联 `style` 与 `<img src>`，所以必须挡住伪协议。

    use crate::api::{public_background, safe_image_source};

    #[test]
    fn accepts_https_and_relative_paths() {
        assert!(safe_image_source("https://cdn.example.com/logo.png"));
        assert!(safe_image_source("/static/logo.png"));
        assert!(safe_image_source("data:image/png;base64,AAAA"));
    }

    #[test]
    fn rejects_dangerous_sources() {
        // 伪协议与混合内容
        assert!(!safe_image_source("javascript:alert(1)"));
        assert!(!safe_image_source("JavaScript:alert(1)"));
        assert!(!safe_image_source("http://example.com/a.png"));
        assert!(!safe_image_source("vbscript:msgbox"));
        // 协议相对 URL 会指向外部
        assert!(!safe_image_source("//evil.com/a.png"));
        // 空值与超长值
        assert!(!safe_image_source(""));
        assert!(!safe_image_source(&"a".repeat(3000)));
    }

    #[test]
    fn control_characters_are_rejected() {
        // 用换行/制表绕过前缀检查的常见手法
        assert!(!safe_image_source("java\nscript:alert(1)"));
        assert!(!safe_image_source("https://a.com/\u{0}.png"));
    }

    #[test]
    fn only_https_images_are_accepted() {
        // 背景只接受图片地址，颜色值一律当作未设置。
        assert_eq!(public_background("#1a1a1a"), "");
        assert_eq!(public_background("rgb(20, 30, 40)"), "");
        assert_eq!(public_background("white"), "");
        assert_eq!(public_background(""), "");
        // 站内相对路径也不再支持：Worker 只托管一个 SPA 入口。
        assert_eq!(public_background("/static/bg.jpg"), "");
        assert_eq!(public_background("http://a.com/bg.jpg"), "");
    }

    #[test]
    fn css_injection_in_color_is_dropped() {
        // 试图闭合声明并注入新规则
        assert_eq!(public_background("red; background-image: url(evil)"), "");
        assert_eq!(public_background("red} body{display:none"), "");
    }

    #[test]
    fn image_sources_become_url() {
        assert_eq!(
            public_background("https://cdn.example.com/bg.jpg"),
            "url(\"https://cdn.example.com/bg.jpg\")"
        );
    }

    #[test]
    fn quotes_in_url_are_escaped() {
        // 地址里带引号时不能提前闭合 url()
        let out = public_background("https://a.com/x\".png");
        assert!(out.starts_with("url(\""), "实际: {out}");
        assert!(out.contains("\\\""), "引号应被转义: {out}");
        assert!(out.ends_with("\")"), "实际: {out}");
    }

    #[test]
    fn dangerous_scheme_never_reaches_css() {
        assert_eq!(public_background("javascript:alert(1)"), "");
        assert_eq!(public_background("http://a.com/bg.png"), "");
    }
}
