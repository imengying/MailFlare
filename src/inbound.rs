//! 收信处理：Email Routing -> mail-parser -> R2 附件 -> D1 -> 异步推送。
//!
//! ## 内存与性能
//! * 原始报文只读一次：`ForwardableEmailMessage::raw_bytes()` 把流收成 `Vec<u8>`，
//!   mail-parser 的 `Message<'x>` 直接借用这块缓冲区（零拷贝）。
//! * 只有需要落库/写 R2 的字段才 `to_owned()`。附件写 R2 时 `Vec<u8>` 直接交给
//!   `Bucket::put`，避免再拷一份。
//! * 超过 `MAX_MESSAGE_BYTES` 的邮件在读取前就 `set_reject`，不让它进入 WASM 堆；
//!   单个附件超限时跳过转存，只在 D1 留下 `stored=false` 的元数据。
//! * 附件写 R2 用 `buffer_unordered` 控制并发度；推送整体挂到 `waitUntil`。

use worker::{console_error, console_log, Bucket, Context, D1Database, Env, ForwardableEmailMessage};

use crate::config::{self, Config};
use crate::db::{self, NewEmail};
use crate::error::{AppError, AppResult};
use crate::mail;
use crate::notify::{self, PushContext, PushEmail};
use crate::r2;
use crate::util::{new_id, now_ms, truncate_utf8};

/// Email 事件入口。
pub async fn handle(
    message: ForwardableEmailMessage,
    env: Env,
    ctx: Context,
) -> worker::Result<()> {
    // 配置缺失时直接拒收，避免静默丢信。
    let cfg = match Config::from_env(&env) {
        Ok(c) => c,
        Err(e) => {
            console_error!("[mailflare] 收信失败：配置缺失 · {e:?}");
            message.set_reject("MailFlare 未完成配置，暂时无法接收邮件");
            return Ok(());
        }
    };

    // 先用 rawSize 判大小：超限时数据根本不会被读进内存。
    let raw_size = message.raw_size();
    if raw_size > cfg.max_message_bytes as f64 {
        console_error!(
            "[mailflare] 邮件过大被拒收：{:.1}MB > {}MB",
            raw_size / 1024.0 / 1024.0,
            cfg.max_message_bytes / 1024 / 1024
        );
        message.set_reject("邮件体积超过 MailFlare 限制");
        return Ok(());
    }

    let envelope_to = message.to();
    let envelope_from = message.from();
    let received_at = now_ms();

    // 只在这里把原始报文拉进内存；之后一路按值传递，避免重复拷贝。
    let raw = message.raw_bytes().await?;

    let db = env.d1("DB")?;
    let bucket = env.bucket("R2")?;

    match process(
        &cfg,
        &db,
        &bucket,
        raw,
        &envelope_to,
        &envelope_from,
        received_at,
    )
    .await
    {
        Ok(Some(email)) => {
            console_log!(
                "[mailflare] 收到邮件 {} · 发件人 {} · {} 个附件",
                email.id,
                email.sender,
                email.attachments.len()
            );
            // 推送不阻塞收信：SMTP 侧已经可以拿到成功响应。
            spawn_push(&cfg, &env, ctx, email);
        }
        Ok(None) => {
            // 重复投递：静默成功，发信方无需重试。
            console_log!("[mailflare] 跳过重复投递的邮件（Message-ID 已存在）");
        }
        Err(e) => {
            console_error!("[mailflare] 邮件入库失败：{e:?}");
            // 让发信方重试，比默默吞掉更可靠。
            message.set_reject("MailFlare 处理邮件时发生内部错误，请稍后重试");
        }
    }

    Ok(())
}

/// 解析 + 写 R2 + 写 D1 的完整流水线。
///
/// `raw` 按值传入：解析只借用它，最后再整体 move 给 R2 保存原文，
/// 全程只有 JS -> Rust 那一次不可避免的拷贝。
async fn process(
    cfg: &Config,
    db: &D1Database,
    bucket: &Bucket,
    raw: Vec<u8>,
    envelope_to: &str,
    envelope_from: &str,
    received_at: i64,
) -> AppResult<Option<db::EmailDetail>> {
    // 解析结果的借用只存在于这一行；`ParsedEmail` 全部字段都是拥有所有权的。
    let Some(parsed) = mail::parse(&raw, cfg.max_body_bytes) else {
        return Err(AppError::BadRequest("邮件格式无法解析".into()));
    };

    let id = new_id();
    let created_at = parsed.date_ms.unwrap_or(received_at);

    // ---- 1. 附件并发写 R2 ----------------------------------------------
    // 先挑出可转存的附件；超限的保留元数据但标记 stored=false，
    // 前端会展示「附件过大，未保存」。
    //
    // 注意 `parsed.attachments` 用 `into_iter()` **消费**掉：附件字节直接
    // move 进 R2，不会在内存里多留一份。超限附件的 Vec 在循环里立刻 drop。
    let mut metas: Vec<db::AttachmentMeta> = Vec::with_capacity(parsed.attachments.len());
    let mut uploads: Vec<(usize, mail::RawAttachment)> = Vec::new();

    for (index, att) in parsed.attachments.into_iter().enumerate() {
        let mut meta = mail::attachment_meta(index, &att);
        if att.bytes.len() > cfg.max_attachment_bytes {
            console_error!(
                "[mailflare] 附件「{}」超过上限（{:.1}MB），跳过转存",
                att.filename,
                att.bytes.len() as f64 / 1024.0 / 1024.0
            );
            // 明确标记未存储：前端据此显示「未保存」，不会给出坏链接。
            meta.stored = false;
            // att 在此 drop，字节立刻归还给分配器。
        } else {
            meta.key = r2::attachment_key(&id, index);
            meta.stored = true;
            uploads.push((index, att));
        }
        metas.push(meta);
    }

    if !uploads.is_empty() {
        use futures_util::StreamExt;

        let email_id: &str = &id;
        let concurrency = r2::effective_concurrency(cfg);

        // `buffer_unordered` 保证既不是串行等待，也不会一次开太多子请求；
        // 每个 future 持有自己的附件所有权，无需额外拷贝。
        let results: Vec<(usize, AppResult<()>)> = futures_util::stream::iter(uploads)
            .map(|(index, att)| async move {
                let key = r2::attachment_key(email_id, index);
                let result = r2::put_attachment(bucket, &key, att, email_id).await;
                (index, result)
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;

        // 单个附件失败不影响整封邮件：只把该项标记为未存储。
        for (index, result) in results {
            if let Err(e) = result {
                console_error!("[mailflare] 附件写入 R2 失败: {e:?}");
                if let Some(meta) = metas.get_mut(index) {
                    meta.stored = false;
                    meta.key.clear();
                }
            }
        }
    }

    // ---- 2. 原始 .eml 存 R2 ---------------------------------------------
    if cfg.store_raw_eml {
        if let Err(e) = r2::put_raw_eml(bucket, &id, raw).await {
            // 原文存不下不算致命错误，元数据仍然有价值。
            console_error!("[mailflare] 原始邮件写入 R2 失败: {e:?}");
        }
    } else {
        // 不保存原文时尽早释放这块可能有 20MB 的缓冲区，降低峰值内存。
        // （附件已经写进 R2，正文也已经是独立的 String。）
        drop(raw);
    }

    // ---- 3. 写 D1 -------------------------------------------------------
    // 信封地址比邮件头更权威：Bcc 投递时 To 头可能是空的。
    let recipient = if envelope_to.is_empty() {
        parsed.recipient.clone()
    } else {
        envelope_to.to_string()
    };
    let sender = if parsed.sender.is_empty() || parsed.sender == "未知发件人" {
        envelope_from.to_string()
    } else {
        parsed.sender.clone()
    };

    let inserted = db::insert_email(
        db,
        &NewEmail {
            id: &id,
            message_id: parsed.message_id.as_deref(),
            sender: &sender,
            recipient: &recipient,
            subject: &parsed.subject,
            text_body: &parsed.text_body,
            html_body: &parsed.html_body,
            attachments: &metas,
            created_at,
        },
    )
    .await?;

    if !inserted {
        // 幂等去重命中：清掉刚写进 R2 的对象，避免留下孤儿。
        r2::delete_email_objects(bucket, &id, &metas, cfg.store_raw_eml).await;
        return Ok(None);
    }

    Ok(Some(db::EmailDetail {
        id,
        message_id: parsed.message_id,
        sender,
        recipient,
        subject: parsed.subject,
        text_body: parsed.text_body,
        html_body: parsed.html_body,
        attachments: metas,
        is_read: false,
        created_at,
    }))
}

/// 把推送任务挂到 `waitUntil` 上。
fn spawn_push(cfg: &Config, env: &Env, ctx: Context, email: db::EmailDetail) {
    let public_url = cfg.public_url.clone();

    // 提前裁好一小段预览：别让几百 KB 的正文一直活在 waitUntil 的 future 里。
    let preview_source = if email.text_body.is_empty() {
        email.html_body
    } else {
        email.text_body
    };
    let push_email = PushEmail {
        id: email.id,
        sender: email.sender,
        subject: email.subject,
        preview: truncate_utf8(preview_source, 2 * 1024),
        attachment_count: email.attachments.len(),
    };

    let Ok(db) = env.d1("DB") else {
        console_error!("[mailflare] 无法访问 D1，跳过推送");
        return;
    };

    ctx.wait_until(async move {
        let settings = match config::load_settings(&db).await {
            Ok(s) => s,
            Err(e) => {
                console_error!("[mailflare] 读取推送配置失败: {e:?}");
                return;
            }
        };
        // 两个渠道都没开就不必再构造消息了。
        if !settings.telegram_enabled && !settings.bark_enabled {
            return;
        }

        let ctx = PushContext {
            settings: &settings,
            email: &push_email,
            public_url: public_url.as_deref(),
        };
        notify::dispatch(&ctx).await;
    });
}
