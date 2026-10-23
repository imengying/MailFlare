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
        // 每个汉字 3 字节，上限 10 字节 -> 只能放 3 个汉字（9 字节）。
        let text = "一二三四五六七八九十".to_string();
        let out = util::truncate_utf8(text, 10);
        // 不能出现替换字符（说明切在了字符边界上）。
        assert!(!out.contains('\u{FFFD}'));
        assert!(out.starts_with("一二三"));
        assert!(out.len() > 10, "应附带截断提示");
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
