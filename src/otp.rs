//! 验证码提取，供邮件详情和通知共用。
//!
//! 候选必须有验证语境；强关键词优先，距离统一按字符计算。
//! 订单号等负面语境优先排除，同等可信但不同的候选不自动选择。

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    pub value: String,
    pub label: &'static str,
}

// 具体关键词放在前面，包含关系只保留更具体的那一个。
const KEYWORDS: &[(&str, &str)] = &[
    ("verification code", "验证码"),
    ("confirmation code", "验证码"),
    ("security code", "安全码"),
    ("login code", "登录码"),
    ("one-time code", "验证码"),
    ("one-time password", "验证码"),
    ("one-time passcode", "验证码"),
    ("one time password", "验证码"),
    ("one time code", "验证码"),
    ("one time passcode", "验证码"),
    ("temporary password", "临时密码"),
    ("passcode", "验证码"),
    ("access code", "验证码"),
    ("authentication code", "验证码"),
    ("sign-in code", "登录码"),
    ("sign in code", "登录码"),
    ("otp", "验证码"),
    ("2fa", "验证码"),
    // 中文：各家的叫法差一个字就漏，「安全代码」与「安全码」是两个不同的串，
    // 必须都列上（微软用前者）。
    ("安全代码", "安全码"),
    ("安全码", "安全码"),
    ("验证码", "验证码"),
    ("验证代码", "验证码"),
    ("动态码", "动态码"),
    ("动态密码", "动态密码"),
    ("校验码", "校验码"),
    ("确认码", "确认码"),
    ("确认代码", "确认码"),
    ("登录码", "登录码"),
    ("一次性密码", "一次性密码"),
    ("临时密码", "临时密码"),
    ("激活码", "激活码"),
];

const WEAK_KEYWORDS: &[(&str, &str)] = &[("confirmation number", "确认码"), ("code", "验证码")];
const NON_OTP: &[&str] = &[
    "postal code",
    "postcode",
    "zip code",
    "barcode",
    "order",
    "invoice",
    "tracking",
    "booking",
    "reservation",
    "coupon",
    "discount",
    "promo",
    "product code",
    "order number",
    "order confirmation number",
    "order confirmation code",
    "booking confirmation code",
    "booking confirmation number",
    "reservation number",
    "invoice number",
    "tracking number",
    "coupon code",
    "discount code",
    "promo code",
    "error code",
    "status code",
    "reference number",
    "copyright",
    "订单号",
    "订单",
    "运单号",
    "运单",
    "物流单号",
    "物流单",
    "邮编",
    "邮政编码",
    "优惠码",
    "折扣码",
    "错误码",
    "产品编号",
    "金额",
    "版权",
];

/// 优先主题，其次纯文本；没有可信结果时才尝试 HTML。
/// 限制分析长度，避免长邮件占用过多 Worker CPU。
pub fn extract(subject: &str, text_body: &str, html_body: &str) -> Option<Code> {
    extract_text(subject)
        .or_else(|| extract_text(text_body))
        .or_else(|| extract_text(&crate::db::html_to_text(html_body)))
}

#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}
impl Span {
    fn distance(self, other: Self) -> usize {
        other
            .start
            .saturating_sub(self.end)
            .max(self.start.saturating_sub(other.end))
    }
    fn contains(self, other: Self) -> bool {
        self.start <= other.start && self.end >= other.end
    }
}

struct Anchor {
    span: Span,
    label: &'static str,
    strong: bool,
}
struct Candidate {
    span: Span,
    value: String,
    strong_only: bool,
}

fn identifier(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn occurrences(chars: &[char], keyword: &str) -> Vec<Span> {
    let needle: Vec<char> = keyword.chars().collect();
    chars
        .windows(needle.len())
        .enumerate()
        .filter_map(|(start, window)| {
            let end = start + needle.len();
            if window != needle
                || (needle[0].is_ascii()
                    && (start > 0 && identifier(chars[start - 1])
                        || end < chars.len() && identifier(chars[end])))
            {
                return None;
            }
            Some(Span { start, end })
        })
        .collect()
}

/// 逗号、句号等通常分隔不同字段；同一句内的直接关联优先。
fn context_distance(chars: &[char], left: Span, right: Span) -> usize {
    let gap = if left.end <= right.start {
        &chars[left.end..right.start]
    } else if right.end <= left.start {
        &chars[right.end..left.start]
    } else {
        &[]
    };
    gap.len()
        + 12 * gap
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    ',' | '，' | ';' | '；' | '.' | '。' | '!' | '！' | '?' | '？'
                )
            })
            .count()
}

fn extract_text(text: &str) -> Option<Code> {
    const MAX_CHARS: usize = 4000;
    // 多读一个字符，以识别被长度上限截断的 token。
    let mut chars: Vec<char> = text.chars().take(MAX_CHARS + 1).collect();
    let truncated = chars.len() > MAX_CHARS;
    chars.truncate(MAX_CHARS);
    let lower: Vec<char> = chars.iter().map(char::to_ascii_lowercase).collect();
    let mut anchors: Vec<Anchor> = Vec::new();
    for (keywords, strong) in [(KEYWORDS, true), (WEAK_KEYWORDS, false)] {
        for &(keyword, label) in keywords {
            for span in occurrences(&lower, keyword) {
                if !anchors.iter().any(|a| a.span.contains(span)) {
                    anchors.push(Anchor {
                        span,
                        label,
                        strong,
                    });
                }
            }
        }
    }
    if anchors.is_empty() {
        return None;
    }
    let negative: Vec<Span> = NON_OTP
        .iter()
        .flat_map(|word| occurrences(&lower, word))
        .collect();
    anchors.retain(|anchor| !negative.iter().any(|span| span.contains(anchor.span)));
    let candidates: Vec<_> = candidates(&chars)
        .into_iter()
        .filter(|candidate| {
            !anchors
                .iter()
                .any(|a| candidate.span.start < a.span.end && candidate.span.end > a.span.start)
        })
        .map(|candidate| {
            let negative_distance = negative
                .iter()
                .map(|span| context_distance(&chars, *span, candidate.span))
                .min();
            (candidate, negative_distance)
        })
        .collect();
    let mut best: Option<((bool, usize), Code)> = None;
    let mut ambiguous = false;
    for anchor in &anchors {
        for (candidate, negative_distance) in &candidates {
            if candidate.strong_only && !anchor.strong
                || truncated && candidate.span.end == chars.len()
            {
                continue;
            }
            let distance = anchor.span.distance(candidate.span);
            let limit = if candidate.strong_only {
                48
            } else if candidate.span.end <= anchor.span.start {
                80
            } else {
                120
            };
            if distance > limit
                || (candidate.span.start < anchor.span.end
                    && candidate.span.end > anchor.span.start)
            {
                continue;
            }
            // 数字更靠近订单号、邮编等标签时，不借附近的验证码关键词误报。
            let proximity = context_distance(&chars, anchor.span, candidate.span);
            if negative_distance.is_some_and(|distance| distance <= proximity) {
                continue;
            }
            let score = (!anchor.strong, proximity);
            match &best {
                Some((rank, code)) if *rank == score => {
                    if code.value != candidate.value {
                        ambiguous = true;
                    }
                }
                Some((rank, _)) if *rank < score => {}
                _ => {
                    best = Some((
                        score,
                        Code {
                            value: candidate.value.clone(),
                            label: anchor.label,
                        },
                    ));
                    ambiguous = false;
                }
            }
        }
    }
    if ambiguous {
        None
    } else {
        best.map(|(_, code)| code)
    }
}

fn horizontal_space(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// 从完整文本切 token，再计算窗口距离；不把窗口边缘的长订单号截成短码。
fn candidates(chars: &[char]) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && (identifier(chars[i]) || chars[i] == '-') {
            i += 1;
        }
        let mut end = i;
        let mut value: String = chars[start..end].iter().collect();
        let mut grouped = false;
        if chars[start..end].iter().all(char::is_ascii_digit) && end - start <= 4 {
            let mut groups = vec![end - start];
            let mut cursor = end;
            loop {
                let separator = cursor;
                while cursor < chars.len() && horizontal_space(chars[cursor]) {
                    cursor += 1;
                }
                if cursor == separator {
                    break;
                }
                let next = cursor;
                while cursor < chars.len() && chars[cursor].is_ascii_digit() {
                    cursor += 1;
                }
                if cursor == next || cursor < chars.len() && identifier(chars[cursor]) {
                    break;
                }
                groups.push(cursor - next);
                end = cursor;
            }
            let digits: usize = groups.iter().sum();
            // 123 456、1234 5678、12 34 56、逐位空格；不合并 123456 + 10分钟。
            grouped = groups.len() > 1
                && (4..=8).contains(&digits)
                && groups.iter().all(|n| *n == groups[0]);
            if grouped {
                value = chars[start..end]
                    .iter()
                    .filter(|c| c.is_ascii_digit())
                    .collect();
                i = end;
            } else if groups.len() > 1 {
                // 无效的长数字分组不能拆出一个看似有效的四位前缀。
                i = end;
                continue;
            } else {
                end = i;
            }
        }
        if !is_code_like(&value) {
            continue;
        }
        // 排除标识符片段、链接、邮箱、金额、小数和带单位的数字。
        let before = start.checked_sub(1).map(|n| chars[n]);
        let after = chars.get(end).copied();
        if before.is_some_and(|c| matches!(c, '_' | '/' | '@' | '=' | '.' | '$' | '¥' | '￥' | '€'))
            || after.is_some_and(|c| matches!(c, '_' | '/' | '@' | '年' | '月' | '日' | '元' | '秒' | '分'))
            // 句末句点允许；小数或域名中的点禁止。
            || after == Some('.') && chars.get(end + 1).is_some_and(|c| c.is_ascii_alphanumeric())
        {
            continue;
        }
        let numeric_len = value.chars().filter(char::is_ascii_digit).count();
        out.push(Candidate {
            span: Span { start, end },
            strong_only: grouped
                || numeric_len == 4 && value.chars().all(|c| c.is_ascii_digit() || c == '-'),
            value,
        });
    }
    out
}

fn is_code_like(token: &str) -> bool {
    let groups: Vec<&str> = token.split('-').collect();
    if groups.iter().any(|g| g.is_empty()) {
        return false;
    }
    let compact = groups.concat();
    if !(4..=8).contains(&compact.len())
        || !compact
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return false;
    }
    if compact.bytes().all(|b| b.is_ascii_digit()) {
        // 数字连字符只接受等长两组，避免把 2026-11-06 当作八位码。
        return groups.len() == 1 || groups.len() == 2 && groups[0].len() == groups[1].len();
    }
    compact.bytes().any(|b| b.is_ascii_digit()) || groups.len() > 1
}

#[cfg(test)]
mod tests {
    use super::Code;

    fn extract(subject: &str, body: &str) -> Option<Code> {
        super::extract(subject, "", body)
    }

    #[test]
    fn finds_code_after_chinese_keyword() {
        let c = extract("【某某】验证码：839201", "").unwrap();
        assert_eq!(c.value, "839201");
        assert_eq!(c.label, "验证码");
    }

    #[test]
    fn finds_code_before_chinese_keyword() {
        let c = extract("", "839201 是您的验证码，5 分钟内有效").unwrap();
        assert_eq!(c.value, "839201");
    }

    #[test]
    fn finds_english_code() {
        let c = extract("Your verification code is 482913", "").unwrap();
        assert_eq!(c.value, "482913");
    }

    #[test]
    fn finds_alphanumeric_code() {
        let c = extract("verification code: A1B2C3", "").unwrap();
        assert_eq!(c.value, "A1B2C3");
    }

    #[test]
    fn ignores_long_numbers() {
        // 订单号比验证码长，不能误报；但同句里的验证码仍应命中。
        assert!(extract("订单号 2024001123456789", "").is_none());
        let c = extract("订单号 2024001123456789，验证码 556677", "").unwrap();
        assert_eq!(c.value, "556677");
    }

    #[test]
    fn ignores_text_without_keyword() {
        // 没有上下文关键词时，纯数字不应被当成验证码。
        assert!(extract("您的账单金额 8848", "").is_none());
        assert!(extract("", "订单 123456 已发货").is_none());
    }

    #[test]
    fn prefers_subject_over_body() {
        let c = extract("验证码 111222", "您的验证码是 333444").unwrap();
        assert_eq!(c.value, "111222");
    }

    #[test]
    fn rejects_lowercase_mix() {
        // 小写字母混排多半是单词或短码，不当作验证码。
        assert!(extract("verification code: abc123", "").is_none());
    }

    #[test]
    fn real_world_samples() {
        // 常见服务商的真实格式。
        assert_eq!(
            extract("Your verification code is 482913", "")
                .unwrap()
                .value,
            "482913"
        );
        assert_eq!(extract("验证码：839201", "").unwrap().value, "839201");
        assert_eq!(
            extract("您的验证码为 668899，5 分钟内有效", "")
                .unwrap()
                .value,
            "668899"
        );
        assert_eq!(
            extract("Apple ID Code: 123456", "").unwrap().value,
            "123456"
        );
        assert_eq!(
            extract("", "Your verification code is 482913")
                .unwrap()
                .value,
            "482913"
        );
        // 只有孤立的「Use 482913 to sign in.」时没有验证码上下文，不猜。
        assert!(extract("", "Use 482913 to sign in.").is_none());

        // 不该误报的场景。
        assert!(extract("订单号 2024001123456789 已发货", "").is_none());
        assert!(extract("您的账单金额 8848 元", "").is_none());
        assert!(extract("会议时间是 14:30", "").is_none());
    }

    #[test]
    fn accepts_four_digits_only_with_verification_context() {
        assert_eq!(extract("验证码 2024", "").unwrap().value, "2024");
        assert!(extract("Copyright 2024", "").is_none());
        assert!(extract("Code 2024", "").is_none());
        // 6 位仍然正常。
        assert_eq!(extract("验证码 202400", "").unwrap().value, "202400");
    }

    #[test]
    fn extracts_from_html_body() {
        // 真实 HTML 邮件：验证码被 <div> 包着，标签会把关键词和数字隔开。
        let html = r#"<h2>登录邮箱验证码</h2>
        <p style="color:#4b5563">请使用以下验证码完成验证。</p>
        <div style="font-size:32px;font-weight:bold;letter-spacing:8px">949746</div>
        <div>有效期：3 分钟</div>"#;
        let c = extract("登录验证码", html).expect("HTML 正文里应能提取");
        assert_eq!(c.value, "949746");
    }

    #[test]
    fn ignores_html_style_and_script() {
        // CSS 里的数字（字号、颜色值）不能被当成验证码。
        let html = r#"<style>.a{font-size:32px;color:#111827;letter-spacing:8px}</style>
        <p>验证码</p><div>123456</div>"#;
        let c = extract("", html).expect("应提取正文里的验证码");
        assert_eq!(c.value, "123456");
    }

    #[test]
    fn real_nediya_email_after_parse() {
        // 用户提供的原始 eml：text/html + quoted-printable，走完整解析链路。
        let raw = include_bytes!("../tests/fixtures/nediya.eml");
        let parsed = crate::mail::parse(raw, 256 * 1024).expect("eml 应能解析");

        // 推送侧优先用 text_body，没有才退回 html_body——两种都要能提取。
        let from_text = extract(&parsed.subject, &parsed.text_body).expect("纯文本路径应能提取");
        assert_eq!(from_text.value, "949746");

        let c = extract(&parsed.subject, &parsed.html_body).expect("应提取到验证码");
        assert_eq!(c.value, "949746");
        assert_eq!(c.label, "验证码");
    }

    #[test]
    fn real_xai_email_with_hyphenated_code() {
        // 验证码是带连字符、纯字母的 `BFM-GXB`，且关键词在后面。
        let raw = include_bytes!("../tests/fixtures/xai.eml");
        let parsed = crate::mail::parse(raw, 256 * 1024).expect("eml 应能解析");

        assert_eq!(parsed.subject, "BFM-GXB xAI confirmation code");
        assert!(parsed.text_body.contains("BFM-GXB"), "纯文本应含验证码");

        let from_text = extract(&parsed.subject, &parsed.text_body).expect("应提取到验证码");
        assert_eq!(from_text.value, "BFM-GXB");
    }

    #[test]
    fn finds_grouped_numeric_code() {
        assert_eq!(extract("验证码：123-456", "").unwrap().value, "123-456");
    }

    #[test]
    fn wider_window_does_not_create_false_positives() {
        // 窗口放宽后，句子里的普通大写词不能被当成验证码。
        assert!(extract("Your order has been SHIPPED and will arrive soon", "").is_none());
        assert!(extract("验证码邮件发送失败，请稍后再试", "").is_none());
        assert!(extract("confirmation code: CONFIRMED", "").is_none());
        // 常见英文词是纯字母 5-8 位，不能被误判成验证码。
        for word in ["THANKS", "PLEASE", "VERIFY", "EMAILS", "LOGIN"] {
            assert!(
                extract(&format!("confirmation code: {word}"), "").is_none(),
                "{word} 不应被识别为验证码"
            );
        }
    }

    #[test]
    fn does_not_treat_short_words_as_codes() {
        // 短单词不能作为验证码。
        assert!(extract("confirmation code: OK", "").is_none());
    }

    #[test]
    fn real_microsoft_security_code() {
        // 微软用的是「安全代码」，不是「安全码」；少一个字就整封漏推。
        let body = "个人 Microsoft 帐户 121@gmail.com 使用以下安全代码。\n\
安全代码：237802\n如果你无法识别个人 Microsoft 帐户";
        assert_eq!(extract("", body).expect("应提取到验证码").value, "237802");
    }

    #[test]
    fn english_phrasings_are_covered() {
        // 各家邮件里真实出现过的英文写法。
        for (subject, body, want) in [
            ("", "Your security code is 123456", "123456"),
            ("", "Use the code below to verify: 123456", "123456"),
            ("", "Your verification code is: 123456", "123456"),
            ("", "Enter this code to sign in: 123456", "123456"),
            ("", "Your passcode is 123456", "123456"),
            ("", "Here is your sign-in code: 123456", "123456"),
            ("", "Your access code is 123456", "123456"),
            ("", "Your authentication code is 123456", "123456"),
            ("", "Your temporary password is 123456", "123456"),
            (
                "",
                "Please enter the verification code sent to your phone: 123456",
                "123456",
            ),
            // 验证码在主题、关键词在正文的常见写法。
            ("123456 is your verification code", "", "123456"),
        ] {
            assert_eq!(
                extract(subject, body).as_ref().map(|c| c.value.as_str()),
                Some(want),
                "{body} 应提取到 {want}"
            );
        }
    }

    #[test]
    fn english_false_positives_stay_rejected() {
        // 没有明确的一次性验证语境时不猜 PIN。
        assert!(extract("", "Your PIN is 1234").is_none());
        // 没有关键词锚定时不能瞎猜，否则正文里任意数字都会被当成验证码。
        assert!(extract("", "Use 123456 to verify your email address").is_none());
    }

    #[test]
    fn handles_case_insensitive_keyword() {
        assert!(extract("VERIFICATION CODE 998877", "").is_some());
        assert!(extract("Verification Code 998877", "").is_some());
    }

    #[test]
    fn ranks_codes_by_context_and_character_distance() {
        for text in [
            "482913 是您的验证码，订单号 998877",
            "订单号 998877，验证码为 482913",
            "482913 是您的验证码，998877 是订单号",
            "482913 是您的验证码，参考数字是 998877",
            "优惠码 998877，登录码：482913",
            "Postal code 100001. Your verification code is 482913",
            "Error code 998877. Verification code: 482913",
            "Verification code: 482913. Order 998877",
            "482913 is your verification code, reference number 998877",
        ] {
            assert_eq!(
                extract(text, "").map(|c| c.value),
                Some("482913".into()),
                "{text}"
            );
        }
        assert_eq!(
            extract("security code: 482913", "").unwrap().label,
            "安全码"
        );
        assert!(extract("验证码：111222；验证码：333444", "").is_none());
        assert_eq!(
            extract("验证码：111222；验证码：111222", "").unwrap().value,
            "111222"
        );
    }

    #[test]
    fn rejects_unrelated_codes_and_identifier_fragments() {
        for text in [
            "Your postal code is 100001",
            "Your postcode is 100001",
            "Your barcode is 12345678",
            "ZIP code: 100001",
            "Coupon code: SAVE20",
            "Discount code: SALE20",
            "Promo code: SALE20",
            "Order confirmation number: 123456",
            "Booking confirmation code: 123456",
            "HTTP error code 123456",
            "Product code: A1B2C3",
            "Verification code delivery failed. Order number: 998877",
            "验证码邮件发送失败，订单号 998877",
            "验证码邮件发送失败，金额 123456元",
            "code: 123456_suffix",
            "code: prefix_123456",
            "code: 123456@example.test",
            "code: https://example.test/123456",
            "code: https://example.test/?id=123456",
            "code: 123456.78",
            "code: $123456",
            "code: 2026-11-06",
            "verification code: 1234 5678 9012",
            "verification code: 2026 11 06",
            "Your HOTP: 123456",
            "Your OTP_backup: 123456",
        ] {
            assert!(extract(text, "").is_none(), "不能误提取: {text}");
        }
        assert_eq!(extract("Your OTP: 482913.", "").unwrap().value, "482913");
    }

    #[test]
    fn supports_short_and_spaced_codes_with_strong_context() {
        for (text, want) in [
            ("验证码：4937", "4937"),
            ("4937 是您的验证码", "4937"),
            ("Your verification code is 0042", "0042"),
            ("Your login code is 123 456", "123456"),
            ("验证码：1 2 3 4 5 6", "123456"),
            ("验证码：12 34 56", "123456"),
            ("验证码：1234 5678", "12345678"),
            ("Your verification code is 123\u{00a0}456", "123456"),
            ("验证码：12-34", "12-34"),
        ] {
            assert_eq!(extract(text, "").unwrap().value, want, "{text}");
        }
        for text in [
            "Code: 4937",
            "Code: 123 456",
            "验证码：123\n456",
            "验证码：123\r\n456",
        ] {
            assert!(super::extract("", text, "").is_none(), "{text}");
        }
        assert_eq!(
            super::extract("", "验证码：123456 10分钟内有效", "")
                .unwrap()
                .value,
            "123456"
        );
    }

    #[test]
    fn html_boundaries_entities_and_inline_digits() {
        for html in [
            "<p>验证码</p><div>482913</div><div>10分钟内有效</div>",
            "<p>验证码</p><table><tr><td>482913</td><td>10分钟内有效</td></tr></table>",
            "<p>验证码：<span>4</span><b>8</b><span>2913</span></p>",
            "<p>verification <b>code</b>: 482913</p>",
            "<div title='>123456'>验证码：482913</div>",
            "<!-- fake > verification code: 111111 --><STYLE>.x{color:#222222}</STYLE><SCRIPT>code=333333</SCRIPT><p>验证码：482913</p>",
            "<p>验证码：&#52;&#56;&#50;&#57;&#49;&#51;</p>",
            "<p>验证码：482&nbsp;913</p>",
        ] {
            assert_eq!(
                super::extract("", "", html).unwrap().value,
                "482913",
                "{html}"
            );
        }
        assert!(super::extract("验证码", "", "<p>123</p><p>456</p>").is_none());
        assert_eq!(
            super::extract("验证码：482913", "", "A&B中文测试邮件")
                .unwrap()
                .value,
            "482913"
        );
        assert_eq!(
            super::extract("", "", "A&B中文测试邮件，验证码：482913")
                .unwrap()
                .value,
            "482913"
        );
    }

    #[test]
    fn falls_back_to_html_and_keeps_plain_text_literal() {
        assert_eq!(
            super::extract(
                "登录验证",
                "请使用 HTML 客户端阅读。",
                "<p>验证码：4937</p>"
            )
            .unwrap()
            .value,
            "4937"
        );
        assert_eq!(
            super::extract("", "Your postal code is 100001", "<p>验证码：482913</p>")
                .unwrap()
                .value,
            "482913"
        );
        assert_eq!(
            super::extract("", "验证码 <4937>", "").unwrap().value,
            "4937"
        );
        assert_eq!(
            super::extract("", "验证码：111222", "<p>验证码：333444</p>")
                .unwrap()
                .value,
            "111222"
        );
        assert_eq!(
            super::extract("验证码：555666", "验证码：111222", "<p>验证码：333444</p>")
                .unwrap()
                .value,
            "555666"
        );
    }

    #[test]
    fn does_not_extract_partial_tokens_at_search_limits() {
        let text = format!(
            "{}123456789012345678901234567890 verification code",
            "中".repeat(70)
        );
        assert!(super::extract("", &text, "").is_none());
        let text = format!("verification code{}1234567890123456", " ".repeat(115));
        assert!(super::extract("", &text, "").is_none());
        let text = format!(
            "{}verification code: 12345678901234567890",
            " ".repeat(3975)
        );
        assert!(super::extract("", &text, "").is_none());
        let text = format!("{}验证码：482913", "测试内容。".repeat(500));
        assert_eq!(super::extract("", &text, "").unwrap().value, "482913");
    }

    #[test]
    fn html_only_mime_keeps_expiry_separate() {
        let raw = b"From: sender@example.test\r\nSubject: Login\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Verification code</p><div>482913</div><div>10 minutes</div>";
        let mail = crate::mail::parse(raw, 256 * 1024).unwrap();
        assert!(!mail.text_body.contains("48291310"));
        assert_eq!(
            super::extract(&mail.subject, &mail.text_body, &mail.html_body)
                .unwrap()
                .value,
            "482913"
        );
    }
}
