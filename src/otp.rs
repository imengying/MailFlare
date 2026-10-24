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
    ("verification code", "验证码"),
    ("security code", "安全码"),
    // 单独的 `code` 很常见（Apple ID Code、login code），但泛用性最强，
    // 所以放在靠后位置，先让更具体的关键词命中。
    ("code", "验证码"),
    ("login code", "登录码"),
    ("one-time code", "验证码"),
    ("one time password", "验证码"),
    ("otp", "验证码"),
    ("验证码", "验证码"),
    ("动态码", "动态码"),
    ("校验码", "校验码"),
    ("确认码", "确认码"),
    ("登录码", "登录码"),
    ("安全码", "安全码"),
    ("激活码", "激活码"),
    ("2fa", "验证码"),
];

/// 从邮件里提取验证码；没有可信结果时返回 `None`。
///
/// 优先看主题（邮件客户端通常把验证码放在主题里），再看正文前若干字符。
pub fn extract(subject: &str, body: &str) -> Option<Code> {
    // 正文只看前 2000 字符：验证码总在开头，没必要扫完整封长邮件。
    let head: String = body.chars().take(2000).collect();

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
/// 关键词后面 40 个字符内优先；找不到再放宽到前面 20 个字符
/// （「123456 是您的验证码」这种句式）。
fn find_near(text: &str, keyword: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(keyword) {
        let at = from + pos;
        let after = at + keyword.len();

        // 关键词之后的 40 个字符。
        let tail: String = text[after..].chars().take(40).collect();
        if let Some(code) = take_code(&tail) {
            return Some(code);
        }

        // 关键词之前的 20 个字符。
        let start = text[..at].char_indices().rev().nth(20).map_or(0, |(i, _)| i);
        let head = &text[start..at];
        if let Some(code) = take_code(head) {
            return Some(code);
        }

        from = after;
        if from >= lower.len() {
            break;
        }
    }
    None
}

/// 从一段文本里取出第一个形似验证码的 token。
///
/// 规则：
/// - 长度 4–8；
/// - 全是数字，或全是大写字母加数字（且至少含一个数字）；
/// - 前后不能紧贴数字/字母，避免截断 `订单2024001` 这种长数字串；
/// - 排除年份、金额这类明显不是验证码的形态。
fn take_code(text: &str) -> Option<String> {
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
            i += 1;
        }
        let token: String = bytes[start..i].iter().collect();

        // 前后必须是非字母数字（当前 token 已经按此切分，这里再挡一次
        // 字母数字混排被下划线衔接的情况）。
        let prev = if start == 0 { None } else { Some(bytes[start - 1]) };
        if matches!(prev, Some('_')) {
            continue;
        }

        if is_code_like(&token) {
            return Some(token);
        }
    }
    None
}

/// 判断一个 token 是否像验证码。
fn is_code_like(token: &str) -> bool {
    let len = token.len();
    if !(4..=8).contains(&len) {
        return false;
    }

    let all_digits = token.chars().all(|c| c.is_ascii_digit());
    if all_digits {
        // 4 位数字太容易是年份、金额、端口，不猜。
        return len != 4;
    }

    // 字母数字混合：只接受全大写（或数字）+ 至少一位数字。
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    let upper_ok = token
        .chars()
        .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase());
    has_digit && upper_ok
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
    fn handles_case_insensitive_keyword() {
        assert!(extract("VERIFICATION CODE 998877", "").is_some());
        assert!(extract("Verification Code 998877", "").is_some());
    }
}
