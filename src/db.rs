//! D1 数据访问层。
//!
//! 只暴露与业务语义对齐的函数，SQL 全部集中在这里，方便审查与优化。
//!
//! ## 关于时间戳
//! `created_at` 存**毫秒** Unix 时间戳。workers-rs 的 `D1Type::Integer` 只有 i32，
//! 毫秒时间戳会溢出，因此统一用 `D1Type::Real(f64)` 绑定（整数在 2^53 内可被 f64
//! 精确表示），写入时再 `CAST(? AS INTEGER)`，保证列里落的是整数而不是浮点。

use serde::{Deserialize, Serialize};
use worker::{D1Database, D1Type};

use crate::error::AppResult;

/// 附件元数据，整体以 JSON 数组存在 `emails.attachments`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentMeta {
    /// MIME 部件在该邮件中的序号，同时作为附件下载 ID 的一部分。
    pub index: usize,
    pub filename: String,
    pub content_type: String,
    /// 解码后的字节数。
    pub size: usize,
    /// R2 对象键；未成功转存（超限/失败）时为空串。
    #[serde(default)]
    pub key: String,
    /// 是否为正文内联资源（`Content-Disposition: inline`，常见于 cid: 图片）。
    #[serde(default)]
    pub inline: bool,
    /// 规范化后的 Content-ID（去掉尖括号），用于把 `cid:` 引用重写成可访问 URL。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cid: Option<String>,
    /// 是否已成功写入 R2。
    #[serde(default = "default_true")]
    pub stored: bool,
}

fn default_true() -> bool {
    true
}

/// 列表页只需要的摘要字段。
#[derive(Debug, Serialize)]
pub struct EmailSummary {
    pub id: String,
    pub sender: String,
    pub subject: String,
    /// 由正文截取的前若干字符，列表里做预览。
    pub preview: String,
    pub is_read: bool,
    pub created_at: i64,
    pub attachment_count: usize,
}

/// 详情页的完整字段。
#[derive(Debug, Serialize)]
pub struct EmailDetail {
    pub id: String,
    pub message_id: Option<String>,
    pub sender: String,
    pub recipient: String,
    pub subject: String,
    pub text_body: String,
    pub html_body: String,
    pub attachments: Vec<AttachmentMeta>,
    pub is_read: bool,
    pub created_at: i64,
}

/// 入库用的完整记录（写入侧用）。
pub struct NewEmail<'a> {
    pub id: &'a str,
    pub message_id: Option<&'a str>,
    pub sender: &'a str,
    pub recipient: &'a str,
    pub subject: &'a str,
    pub text_body: &'a str,
    pub html_body: &'a str,
    pub attachments: &'a [AttachmentMeta],
    pub created_at: i64,
}

/// 列表查询参数。
pub struct ListQuery {
    pub unread_only: bool,
    /// 游标：(created_at, id)
    pub cursor: Option<(i64, String)>,
    pub limit: i64,
}

/// 毫秒时间戳 -> D1 绑定值（见模块注释）。
#[inline]
fn ts(v: i64) -> D1Type<'static> {
    D1Type::Real(v as f64)
}

/// 反序列化 D1 返回的列表行。
#[derive(Deserialize)]
struct SummaryRow {
    id: String,
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    preview: Option<String>,
    /// 1 表示 preview 来自 HTML 正文，需要去标签后再展示。
    #[serde(default)]
    preview_is_html: Option<i64>,
    #[serde(default)]
    is_read: Option<i64>,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    attachments: Option<String>,
}

/// 反序列化 D1 返回的详情行。
#[derive(Deserialize)]
struct DetailRow {
    id: String,
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    recipient: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    text_body: Option<String>,
    #[serde(default)]
    html_body: Option<String>,
    #[serde(default)]
    attachments: Option<String>,
    #[serde(default)]
    is_read: Option<i64>,
    #[serde(default)]
    created_at: Option<i64>,
}

/// 解析 attachments JSON；损坏时返回空数组而不是报错，保证邮件本身还能看。
fn parse_attachments(raw: Option<&str>) -> Vec<AttachmentMeta> {
    match raw {
        Some(s) if !s.is_empty() => serde_json::from_str(s).unwrap_or_else(|e| {
            worker::console_error!("[mailflare] attachments JSON 解析失败: {e}");
            Vec::new()
        }),
        _ => Vec::new(),
    }
}

/// 把连续空白压成单个空格，列表预览更紧凑。
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim_end().to_string()
}

/// 极简 HTML 去标签，只用于列表预览（不追求语义正确）。
///
/// 特点是**单遍扫描、不分配中间字符串**：遇到 `<` 跳到下一个 `>`；顺带把
/// `<style>`/`<script>` 的内容整段丢掉，避免预览里出现一堆 CSS。
fn strip_html_preview(html: &str) -> String {
    let mut out = String::with_capacity(html.len().min(512));
    let bytes = html.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            // 普通文本：按字符推进，保证不会切在 UTF-8 边界上。
            let ch = html[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        let tag_end = html[i..].find('>').map(|p| i + p).unwrap_or(bytes.len());
        let tag = &html[i + 1..tag_end.min(bytes.len())];
        i = (tag_end + 1).min(bytes.len());

        // 跳过 style / script 的整段内容。
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '>')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if !tag.starts_with('/') && (name == "style" || name == "script") {
            let close = if name == "style" { "</style" } else { "</script" };
            if let Some(pos) = html[i..].find(close) {
                i += pos;
                i = html[i..].find('>').map(|p| i + p + 1).unwrap_or(bytes.len());
            }
        }
    }

    collapse_whitespace(&out)
}

/// 仅供测试：让单元测试能直接验证 HTML 去标签逻辑（私有函数不便导出）。
#[cfg(test)]
pub fn strip_html_preview_for_test(html: &str) -> String {
    strip_html_preview(html)
}

/// 插入邮件；返回 `true` 表示确实是新邮件（`false` 说明 message_id 重复被忽略）。
pub async fn insert_email(db: &D1Database, email: &NewEmail<'_>) -> AppResult<bool> {
    let attachments_json = serde_json::to_string(email.attachments)?;

    // D1 单行上限约 1MB。正文在上游已按 MAX_BODY_BYTES（默认 256KB，
    // 上限 900KB）截断，但 text + html + attachments JSON 是**相加**的，
    // 两段正文都顶到上限时仍可能超。这里按行总长再做一次兜底截断，
    // 宁可丢一点尾部，也不能让整条 INSERT 失败（那会连元数据一起丢）。
    const MAX_ROW_BYTES: usize = 1_000_000;
    let fixed = email.id.len()
        + email.message_id.map_or(0, |v| v.len())
        + email.sender.len()
        + email.recipient.len()
        + email.subject.len()
        + attachments_json.len();
    let body_budget = MAX_ROW_BYTES.saturating_sub(fixed);

    // 两段正文平分剩余预算；若某段更短，把富余让给另一段。
    let text_body = crate::util::truncate_utf8(email.text_body.to_string(), body_budget / 2);
    let remaining = body_budget.saturating_sub(text_body.len());
    let html_body = crate::util::truncate_utf8(email.html_body.to_string(), remaining);

    let stmt = db
        .prepare(
            "INSERT OR IGNORE INTO emails \
             (id, message_id, sender, recipient, subject, text_body, html_body, attachments, is_read, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, CAST(?9 AS INTEGER))",
        )
        .bind_refs(&[
            D1Type::Text(email.id),
            match email.message_id {
                Some(v) => D1Type::Text(v),
                None => D1Type::Null,
            },
            D1Type::Text(email.sender),
            D1Type::Text(email.recipient),
            D1Type::Text(email.subject),
            D1Type::Text(&text_body),
            D1Type::Text(&html_body),
            D1Type::Text(&attachments_json),
            ts(email.created_at),
        ])?;

    let result = stmt.run().await?;
    if !result.success() {
        return Err(crate::error::AppError::Internal(format!(
            "D1 插入失败: {}",
            result.error().unwrap_or_default()
        )));
    }
    // 只有「明确返回 changes = 0」才认定为重复投递。
    // 不能用 `is_some_and(|n| n > 0)`：万一 D1 没返回 meta，会被误判成重复，
    // 于是刚写进 R2 的对象被删掉、邮件被静默丢弃。
    let changes = result.meta()?.and_then(|m| m.changes);
    Ok(changes != Some(0))
}

/// 分页查询邮件列表。
///
/// 返回 `(本页数据, 下一页游标, 未读总数)`。游标分页避免了 OFFSET 越翻越慢。
pub async fn list_emails(
    db: &D1Database,
    query: &ListQuery,
) -> AppResult<(Vec<EmailSummary>, Option<String>, i64)> {
    // SQL 片段是编译期常量，参数全部走绑定，不存在注入面。
    // preview 优先用纯文本正文；只有纯文本为空时才退回 HTML（多带一个
    // `preview_is_html` 标记，好在 Rust 侧决定要不要去标签）。
    // substr 在 SQLite 里按字符计数，中文不会被切坏。
    let mut sql = String::from(
        "SELECT id, sender, subject, \
         substr(COALESCE(NULLIF(text_body, ''), html_body, ''), 1, 240) AS preview, \
         CASE WHEN COALESCE(text_body, '') = '' THEN 1 ELSE 0 END AS preview_is_html, \
         is_read, created_at, attachments \
         FROM emails WHERE 1 = 1",
    );

    if query.unread_only {
        sql.push_str(" AND is_read = 0");
    }
    if query.cursor.is_some() {
        // 用 (created_at, id) 复合游标保证同一毫秒内的记录也能稳定翻页。
        sql.push_str(" AND (created_at < ? OR (created_at = ? AND id < ?))");
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");

    let mut binds: Vec<D1Type<'_>> = Vec::with_capacity(4);
    if let Some((created_at, id)) = &query.cursor {
        binds.push(ts(*created_at));
        binds.push(ts(*created_at));
        binds.push(D1Type::Text(id));
    }
    // 多取一条，用来判断是否还有下一页。
    binds.push(D1Type::Integer((query.limit + 1) as i32));

    // 列表与未读计数合并进一次 `batch`：D1 的每条语句都是一次网络往返，
    // 合成一个 batch 能把侧边栏角标的延迟从 2 RTT 降到 1 RTT。
    let list_stmt = db.prepare(sql).bind_refs(&binds)?;
    let count_stmt = db.prepare("SELECT COUNT(*) AS c FROM emails WHERE is_read = 0");

    let mut results = db.batch(vec![list_stmt, count_stmt]).await?;
    // 用 `match` 而不是先检查长度再 pop：即使 D1 行为异常也只会返回 500，
    // 不会 panic（panic 在 Worker 里会直接终止整个请求，且难以定位）。
    let (Some(count_result), Some(result)) = (results.pop(), results.pop()) else {
        return Err(crate::error::AppError::Internal(
            "D1 batch 返回结果数量异常".into(),
        ));
    };

    if !result.success() {
        return Err(crate::error::AppError::Internal(format!(
            "D1 查询失败: {}",
            result.error().unwrap_or_default()
        )));
    }

    let unread: i64 = count_result
        .results::<CountRow>()?
        .first()
        .map(|r| r.c)
        .unwrap_or(0);

    let mut rows: Vec<SummaryRow> = result.results()?;
    let has_more = rows.len() as i64 > query.limit;
    if has_more {
        rows.truncate(query.limit as usize);
    }

    let next_cursor = if has_more {
        rows.last().map(|r| {
            format!(
                "{}_{}",
                r.created_at.unwrap_or_default(),
                r.id.as_str()
            )
        })
    } else {
        None
    };

    let items = rows
        .into_iter()
        .map(|r| {
            let attachments = parse_attachments(r.attachments.as_deref());
            // 内联图片不计入「附件」角标，和前端详情页的口径保持一致。
            let file_count = attachments.iter().filter(|a| !a.inline).count();
            let raw_preview = r.preview.unwrap_or_default();
            let preview = if r.preview_is_html.unwrap_or(0) != 0 {
                strip_html_preview(&raw_preview)
            } else {
                collapse_whitespace(&raw_preview)
            };

            EmailSummary {
                attachment_count: file_count,
                id: r.id,
                sender: r.sender.unwrap_or_default(),
                subject: r.subject.unwrap_or_else(|| "(无主题)".to_string()),
                preview,
                is_read: r.is_read.unwrap_or(0) != 0,
                created_at: r.created_at.unwrap_or_default(),
            }
        })
        .collect();

    Ok((items, next_cursor, unread))
}

/// `SELECT COUNT(*) AS c` 的最小反序列化目标。
#[derive(Deserialize)]
struct CountRow {
    c: i64,
}

/// 读取单封邮件详情。
pub async fn get_email(db: &D1Database, id: &str) -> AppResult<Option<EmailDetail>> {
    let stmt = db
        .prepare(
            "SELECT id, message_id, sender, recipient, subject, text_body, html_body, \
             attachments, is_read, created_at FROM emails WHERE id = ?1",
        )
        .bind_refs(&[D1Type::Text(id)])?;

    let row: Option<DetailRow> = stmt.first(None).await?;
    Ok(row.map(|r| EmailDetail {
        attachments: parse_attachments(r.attachments.as_deref()),
        id: r.id,
        message_id: r.message_id,
        sender: r.sender.unwrap_or_default(),
        recipient: r.recipient.unwrap_or_default(),
        subject: r.subject.unwrap_or_else(|| "(无主题)".to_string()),
        text_body: r.text_body.unwrap_or_default(),
        html_body: r.html_body.unwrap_or_default(),
        is_read: r.is_read.unwrap_or(0) != 0,
        created_at: r.created_at.unwrap_or_default(),
    }))
}

/// 只查 `attachments` 列时的最小行结构。
///
/// 不能直接 `first::<String>(Some("attachments"))`：列值可能是 NULL，
/// serde 反序列化会失败；而且 `first` 无法区分「行不存在」与「列为 NULL」。
#[derive(Deserialize)]
struct AttachmentsRow {
    #[serde(default)]
    attachments: Option<String>,
}

/// 只取附件元数据（下载与删除时校验归属用）。
///
/// 返回 `None` 表示邮件不存在；`Some(vec![])` 表示存在但没有附件。
pub async fn get_attachments(
    db: &D1Database,
    id: &str,
) -> AppResult<Option<Vec<AttachmentMeta>>> {
    let stmt = db
        .prepare("SELECT attachments FROM emails WHERE id = ?1")
        .bind_refs(&[D1Type::Text(id)])?;
    let row: Option<AttachmentsRow> = stmt.first(None).await?;
    Ok(row.map(|r| parse_attachments(r.attachments.as_deref())))
}

/// 标记已读/未读。
pub async fn set_read(db: &D1Database, id: &str, read: bool) -> AppResult<bool> {
    let stmt = db
        .prepare("UPDATE emails SET is_read = ?1 WHERE id = ?2")
        .bind_refs(&[D1Type::Integer(i32::from(read)), D1Type::Text(id)])?;
    let result = stmt.run().await?;
    // 同上：只有明确 0 行受影响才认为邮件不存在。
    Ok(result.meta()?.and_then(|m| m.changes) != Some(0))
}

/// 把所有未读邮件标为已读，返回受影响行数。
///
/// 加 `WHERE is_read = 0` 是必要的：全表 UPDATE 会让每一行都产生写放大
/// （D1 按行计费），而这里通常只有少量未读。
pub async fn mark_all_read(db: &D1Database) -> AppResult<i64> {
    let result = db
        .prepare("UPDATE emails SET is_read = 1 WHERE is_read = 0")
        .run()
        .await?;
    if !result.success() {
        return Err(crate::error::AppError::Internal(format!(
            "D1 更新失败: {}",
            result.error().unwrap_or_default()
        )));
    }
    Ok(result.meta()?.and_then(|m| m.changes).unwrap_or(0) as i64)
}

/// 删除 D1 记录，返回被删除邮件的附件元数据（调用方据此清理 R2）。
pub async fn delete_email(db: &D1Database, id: &str) -> AppResult<Option<Vec<AttachmentMeta>>> {
    let attachments = get_attachments(db, id).await?;
    if attachments.is_none() {
        return Ok(None);
    }

    db.prepare("DELETE FROM emails WHERE id = ?1")
        .bind_refs(&[D1Type::Text(id)])?
        .run()
        .await?;

    Ok(attachments)
}
