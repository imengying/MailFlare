//! 收信处理：Email Routing -> mail-parser -> R2 附件 -> D1 -> 异步推送。
//!
//! 内存要点：
//! * 原始报文只读一次，`Message<'_>` 直接借用缓冲区（零拷贝）；
//! * 附件字节 move 进请求体，不额外拷贝；
//! * 黑白名单在读取报文**之前**判断，只比对信封地址；
//! * 超过单封上限的邮件在读取前 `set_reject`，不进入 WASM 堆；
//! * 附件并发写 R2 用 `buffer_unordered`，推送挂 `waitUntil`。
//!
//! D1 存完整邮件（含正文与附件元数据），R2 只存附件字节。

use worker::{console_error, console_log, Context, D1Database, Env, ForwardableEmailMessage};

use crate::config::{self, Config, Settings};
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
    // 信封地址要在任何提前返回之前取到：告警消息里需要它。
    let envelope_to = message.to();
    let envelope_from = message.from();
    let raw_size = message.raw_size();

    // 账号配置缺失时直接拒收。此时无法告警（推送配置也在 D1 里，
    // 很可能连 D1 绑定都没配好），只能写日志——运维应该盯 Worker 日志。
    if let Err(e) = Config::from_env(&env) {
        console_error!("[mailflare] 收信失败：配置缺失 · {e:?}");
        message.set_reject("MailFlare 未完成配置，暂时无法接收邮件");
        return Ok(());
    }

    // D1 句柄提前取：既给正常流程用，也给失败告警用。
    // 拿不到就直接返回错误（无法告警也无法入库）。
    let db = env.d1("DB")?;

    // 收信参数全部来自 D1；读取失败时退回默认值而不是拒收，
    // 宁可多存一封邮件，也不要因为设置表读不出来而丢信。
    let cfg = config::load_settings(&db).await.unwrap_or_else(|e| {
        console_error!("[mailflare] 读取设置失败，使用默认值继续收信: {e:?}");
        Settings::default()
    });

    // ---- 黑名单 / 白名单 ------------------------------------------------
    // 放在体积检查之前：垃圾邮件往往带大附件，先按地址拦掉能省下
    // 后面所有判断的成本。两者都只看信封地址，不解析报文。
    if config::sender_blocked(&cfg.blocked_senders, &envelope_from) {
        // 黑名单命中：静默丢弃，不退信。
        // 退信等于告诉对方「这个地址真实存在」，反而助长后续骚扰。
        console_log!("[mailflare] 发件人在黑名单中，已丢弃：{envelope_from}");
        return Ok(());
    }
    if !config::recipient_allowed(&cfg.allowed_recipients, &envelope_to) {
        // 白名单非空但没匹配上：同样静默丢弃。
        console_log!("[mailflare] 收件人不在白名单中，已丢弃：{envelope_to}");
        return Ok(());
    }

    // ---- 体积检查 ------------------------------------------------------
    // 超限邮件在**读取之前**就拒收，数据不会进入 WASM 堆。
    if raw_size > cfg.max_message_bytes as f64 {
        let limit_mb = cfg.max_message_bytes / 1024 / 1024;
        let detail = format!("{:.2} MB 超过上限 {limit_mb} MB", raw_size / 1024.0 / 1024.0);
        console_error!("[mailflare] 邮件过大被拒收：{detail}");

        // 超大邮件往往是「有人误发了大附件」，值得告警一次——
        // 否则发件人只会看到退信，而你完全不知道发生过。
        spawn_alert(
            &ctx,
            &env,
            notify::InboundFailure::TooLarge,
            detail,
            &envelope_from,
            &envelope_to,
            raw_size,
        );
        message.set_reject("邮件体积超过 MailFlare 限制");
        return Ok(());
    }

    let received_at = now_ms();

    // ---- 读取原始报文 --------------------------------------------------
    // 这里是唯一会把整封邮件拉进内存的地方。
    let raw = match message.raw_bytes().await {
        Ok(raw) => raw,
        Err(e) => {
            let detail = format!("读取邮件原始内容失败：{e}");
            console_error!("[mailflare] {detail}");
            spawn_alert(
                &ctx,
                &env,
                notify::InboundFailure::StorageError,
                detail,
                &envelope_from,
                &envelope_to,
                raw_size,
            );
            message.set_reject("MailFlare 读取邮件失败，请稍后重试");
            return Ok(());
        }
    };

    // ---- 解析 + 落库 ---------------------------------------------------
    match process(
        &cfg,
        &db,
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
            // 这是**预期行为**，不该告警（SMTP 重试很常见）。
            console_log!("[mailflare] 跳过重复投递的邮件（Message-ID 已存在）");
        }
        Err(e) => {
            // 区分「邮件本身有问题」与「我方后端故障」：
            // 前者告警意义不大（垃圾邮件/畸形邮件很常见），后者必须告警。
            let (failure, detail) = match &e {
                AppError::BadRequest(msg) => (
                    notify::InboundFailure::Unparsable,
                    format!("邮件解析失败：{msg}"),
                ),
                other => (
                    notify::InboundFailure::StorageError,
                    format!("邮件入库失败：{other:?}"),
                ),
            };
            console_error!("[mailflare] {detail}");

            if matches!(failure, notify::InboundFailure::StorageError) {
                spawn_alert(
                    &ctx,
                    &env,
                    failure,
                    detail,
                    &envelope_from,
                    &envelope_to,
                    raw_size,
                );
            }

            // 让发信方重试，比默默吞掉更可靠。
            message.set_reject("MailFlare 处理邮件时发生内部错误，请稍后重试");
        }
    }

    Ok(())
}

/// 把告警挂到 `waitUntil`，不阻塞 SMTP 响应。
fn spawn_alert(
    ctx: &Context,
    env: &Env,
    failure: notify::InboundFailure,
    detail: String,
    envelope_from: &str,
    envelope_to: &str,
    raw_size: f64,
) {
    // `D1Database` 没有实现 `Clone`，不能直接捕获到 'static 的异步块里。
    // `Env` 是 Clone 的（内部就是 JsValue），把它复制一份带进去，
    // 在异步块里重新 `env.d1("DB")` 取绑定即可——这一步只是读一个 JS 属性。
    let env = env.clone();
    let from = envelope_from.to_string();
    let to = envelope_to.to_string();

    ctx.wait_until(async move {
        let Ok(db) = env.d1("DB") else {
            console_error!("[mailflare] 告警失败：无法访问 D1");
            return;
        };
        notify::alert_inbound_failure(&db, failure, &detail, &from, &to, raw_size).await;
    });
}

/// 解析 + 写 R2 + 写 D1 的完整流水线。
///
/// `raw` 按值传入：解析只借用它，最后再整体 move 给 R2 保存原文，
/// 全程只有 JS -> Rust 那一次不可避免的拷贝。
async fn process(
    cfg: &Settings,
    db: &D1Database,
    raw: Vec<u8>,
    envelope_to: &str,
    envelope_from: &str,
    received_at: i64,
) -> AppResult<Option<db::EmailDetail>> {
    // 解析结果的借用只存在于这一行；`ParsedEmail` 全部字段都是拥有所有权的。
    let Some(parsed) = mail::parse(&raw, cfg.max_body_bytes as usize) else {
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
        if att.bytes.len() > cfg.max_attachment_bytes as usize {
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
                let result = r2::put_attachment(cfg, &key, att, email_id).await;
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

    // 原始报文到这里已经用完了：正文/附件都是拥有所有权的独立数据。
    // 尽早释放这块可能有 20MB 的缓冲区，降低峰值内存。
    drop(raw);

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
        r2::delete_email_objects(cfg, &metas).await;
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
fn spawn_push(settings: &Settings, env: &Env, ctx: Context, email: db::EmailDetail) {
    let public_url = settings.public_url().map(|s| s.to_string());

    // 提前裁好一小段预览：别让几百 KB 的正文一直活在 waitUntil 的 future 里。
    let preview_source = if email.text_body.is_empty() {
        email.html_body
    } else {
        email.text_body
    };
    // 验证码只在推送时提取一次，正文随后就被丢弃。
    let code = settings
        .otp_enabled
        .then(|| crate::otp::extract(&email.subject, &preview_source))
        .flatten();
    let push_email = PushEmail {
        id: email.id,
        sender: email.sender,
        subject: email.subject,
        preview: truncate_utf8(preview_source, 2 * 1024),
        attachment_count: email.attachments.len(),
        code,
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
