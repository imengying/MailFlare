//! 验证码提取。
//!
//! 只在推送阶段使用：从主题和正文里找一次性的数字/字母码，
//! 命中后由 `notify` 把它单独置顶显示，省得再去翻邮件。
//!
//! 设计约束：
//! - **宁缺毋滥**。漏掉一个验证码只是少了个便利，误报却会把普通数字
//!   （订单号、金额、日期）当成验证码推给用户，反而干扰判断。
//! - 不依赖具体站点，靠「验证码/验证码是/verification code」这类
//!   上下文关键词定位，而不是纯正则扫全局数字。

/// 提取出的验证码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    pub value: String,
    /// 该码所属的场景，用于通知里做小标题（如「验证码」「登录码」）。
    pub label: &'static str,
}

/// 上下文关键词：`(关键词, 展示标签)`。
///
/// 顺序有意义：越具体的越靠前，命中即返回。
const KEYWORDS: &[(&str, &str)] = &[
    // 顺序有意义：`extract` 按这个表依次尝试，**更具体的必须排在泛用的前面**，
    // 否则「login code: 123456」会先被 `code` 命中，标签也跟着变成通用文案。
    ("verification code", "验证码"),
    ("confirmation code", "验证码"),
    ("confirmation number", "确认码"),
    ("security code", "安全码"),
    ("login code", "登录码"),
    ("one-time code", "验证码"),
    ("one time password", "验证码"),
    // 泛用词放最后：Apple ID Code、login code 这类都靠它兜底。
    ("code", "验证码"),
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

/// 从邮件里提取验证码；没有可信结果时返回 `None`。
///
/// 优先看主题（邮件客户端通常把验证码放在主题里），再看正文前若干字符。
///
/// 正文可能是 HTML：调用方无法可靠区分，所以这里统一做一次「去标签」。
/// 验证码常被 `<div>` 之类的标签包着，不去标签会让关键词和数字被标签隔开。
pub fn extract(subject: &str, body: &str) -> Option<Code> {
    // 去标签后再截断：先截断可能把标签截成半截，反而更难处理。
    let plain = crate::db::strip_html_preview(body);
    // 正文只看前 2000 字符：验证码总在开头，没必要扫完整封长邮件。
    let head: String = plain.chars().take(2000).collect();

    for text in [subject, head.as_str()] {
        for (keyword, label) in KEYWORDS {
            if let Some(code) = find_near(text, keyword) {
                return Some(Code { value: code, label });
            }
        }
    }
    None
}

/// 在关键词附近找一个像验证码的串。
///
/// 取窗口内**距离关键词最近**的候选，而不是固定「先看后面再看前面」：
/// `BFM-GXB xAI confirmation code` 这种验证码在前、关键词在后的写法很常见，
/// 固定顺序会先撞上后面的无关数字。
fn find_near(text: &str, keyword: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    // 关键词前后的搜索窗口。真实邮件常写成
    // 「Please use the code below to validate your email address. BFM-GXB」，
    // 关键词与验证码之间隔着半句话，窗口太窄会漏；命中时按距离取最近的一个，
    // 所以放宽窗口不会明显增加误报。
    const WINDOW_BEFORE: usize = 80;
    const WINDOW_AFTER: usize = 120;

    let mut best: Option<(usize, String)> = None;
    let mut from = 0;
    while let Some(pos) = lower[from..].find(keyword) {
        let at = from + pos;
        let after = at + keyword.len();

        // 关键词之前：越靠近关键词的候选越优先。
        let start = text[..at]
            .char_indices()
            .rev()
            .nth(WINDOW_BEFORE)
            .map_or(0, |(i, _)| i);
        let head = &text[start..at];
        for (offset, code) in candidates(head) {
            // 距离 = 候选结束位置到关键词的距离（head 内越靠后越近）。
            let distance = head.len() - offset;
            if best.as_ref().is_none_or(|(d, _)| distance < *d) {
                best = Some((distance, code));
            }
        }

        // 关键词之后。
        let tail: String = text[after..].chars().take(WINDOW_AFTER).collect();
        for (offset, code) in candidates(&tail) {
            let distance = offset;
            if best.as_ref().is_none_or(|(d, _)| distance < *d) {
                best = Some((distance, code));
            }
        }

        from = after;
        if from >= lower.len() {
            break;
        }
    }
    best.map(|(_, code)| code)
}

/// 列出文本中所有形似验证码的候选，附带其在文本中的起始偏移。
fn candidates(text: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        if !chars[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        // 允许内部的连字符：`BFM-GXB`、`123-456` 这类分组码很常见。
        // 连字符必须夹在两个字母数字之间，不能收尾。
        while i < chars.len() {
            let boundary_dash = chars[i] == '-'
                && i > start
                && i + 1 < chars.len()
                && chars[i + 1].is_ascii_alphanumeric();
            if !chars[i].is_ascii_alphanumeric() && !boundary_dash {
                break;
            }
            i += 1;
        }

        let token: String = chars[start..i].iter().collect();
        // 紧贴字母数字的 token 会被切分，这里再挡一次下划线衔接。
        if start > 0 && chars[start - 1] == '_' {
            continue;
        }
        if is_code_like(&token) {
            out.push((start, token));
        }
    }
    out
}

/// 判断一个 token 是否像验证码。
fn is_code_like(token: &str) -> bool {
    // 去掉分组连字符后再评估长度与字符集。
    let compact: String = token.chars().filter(|c| *c != '-').collect();
    let len = compact.chars().count();
    if !(4..=8).contains(&len) {
        return false;
    }

    let all_digits = compact.chars().all(|c| c.is_ascii_digit());
    if all_digits {
        // 4 位数字太容易是年份、金额、端口，不猜。
        return len != 4;
    }

    // 字母数字混合：全大写（或数字）。
    let has_alpha = compact.chars().any(|c| c.is_ascii_uppercase());
    let has_digit = compact.chars().any(|c| c.is_ascii_digit());
    let upper_ok = compact
        .chars()
        .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase());
    if !upper_ok || !has_alpha {
        return false;
    }

    // 含数字：`A1B2C3`、`XYZ789` 这类基本可信。
    if has_digit {
        return true;
    }

    // 纯字母：只有带分组连字符时才接受（`BFM-GXB`）。
    // 否则 `THANKS`、`PLEASE`、`VERIFY` 这些普通单词会被误判成验证码。
    compact.len() != token.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(extract("Your verification code is 482913", "").unwrap().value, "482913");
        assert_eq!(extract("验证码：839201", "").unwrap().value, "839201");
        assert_eq!(extract("您的验证码为 668899，5 分钟内有效", "").unwrap().value, "668899");
        assert_eq!(extract("Apple ID Code: 123456", "").unwrap().value, "123456");
        assert_eq!(extract("", "Your verification code is 482913").unwrap().value, "482913");
        // 只有孤立的「Use 482913 to sign in.」时没有验证码上下文，不猜。
        assert!(extract("", "Use 482913 to sign in.").is_none());

        // 不该误报的场景。
        assert!(extract("订单号 2024001123456789 已发货", "").is_none());
        assert!(extract("您的账单金额 8848 元", "").is_none());
        assert!(extract("会议时间是 14:30", "").is_none());
    }

    #[test]
    fn ignores_four_digit_numbers() {
        // 4 位数字太像年份/端口/金额，宁可漏掉也不误报。
        assert!(extract("验证码 2024", "").is_none());
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

        println!("subject   = {:?}", parsed.subject);
        println!("text_body = {:?}", parsed.text_body);
        println!("html 前 200 = {:?}", &parsed.html_body[..parsed.html_body.len().min(200)]);

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
        // 纯字母码要求至少 5 位，避免 OK / PDF 之类误报。
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
    fn handles_case_insensitive_keyword() {
        assert!(extract("VERIFICATION CODE 998877", "").is_some());
        assert!(extract("Verification Code 998877", "").is_some());
    }
}
