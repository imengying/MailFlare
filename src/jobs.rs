//! D1 持久化恢复任务。每个任务原子领取租约，避免 email / cron 并发重复执行。
use crate::{config, db, error::AppResult, inbound, notify, r2, util};
use serde::Deserialize;
use worker::{D1Database, D1Type, Env, console_error};

pub async fn register_inbound(
    db: &D1Database,
    id: &str,
    from: &str,
    to: &str,
    at: i64,
) -> AppResult<bool> {
    #[derive(Deserialize)]
    struct Registered {
        id: String,
    }
    let row: Option<Registered> = db.prepare("INSERT OR IGNORE INTO inbound_jobs (id, envelope_from, envelope_to, received_at, available_at, lease_until) VALUES (?1, ?2, ?3, ?4, ?5, ?5 + 600) RETURNING id")
        .bind_refs(&[D1Type::Text(id), D1Type::Text(from), D1Type::Text(to), D1Type::Real(at as f64), D1Type::Real(util::now_secs() as f64)])?.first(None).await?;
    Ok(row.is_some_and(|row| row.id == id))
}
pub async fn release_inbound(db: &D1Database, id: &str) -> AppResult<()> {
    db.prepare("UPDATE inbound_jobs SET lease_until = 0, available_at = ?1 + 60 WHERE id = ?2")
        .bind_refs(&[D1Type::Real(util::now_secs() as f64), D1Type::Text(id)])?
        .run()
        .await?;
    Ok(())
}
pub async fn finish_inbound(db: &D1Database, id: &str) -> AppResult<()> {
    db.prepare("DELETE FROM inbound_jobs WHERE id = ?1")
        .bind_refs(&[D1Type::Text(id)])?
        .run()
        .await?;
    Ok(())
}

pub async fn enqueue_notifications(
    db: &D1Database,
    settings: &config::Settings,
    email: &db::EmailDetail,
    received_at: i64,
) -> AppResult<()> {
    if crate::otp::extract(&email.subject, &email.text_body, &email.html_body).is_none() {
        return Ok(());
    }
    let expires = received_at / 1000 + 600;
    let now = util::now_secs();
    if expires <= now || email.deleted_at.is_some() {
        return Ok(());
    }
    for (channel, enabled) in [
        ("telegram", settings.telegram_enabled),
        ("bark", settings.bark_enabled),
    ] {
        if enabled {
            db.prepare("INSERT OR IGNORE INTO notification_jobs (email_id, channel, available_at, expires_at) VALUES (?1, ?2, ?3, ?4)")
                .bind_refs(&[D1Type::Text(&email.id),D1Type::Text(channel),D1Type::Real(now as f64),D1Type::Real(expires as f64)])?.run().await?;
        }
    }
    db.prepare("UPDATE emails SET push_status = 'pending' WHERE id = ?1 AND EXISTS (SELECT 1 FROM notification_jobs WHERE email_id = ?1 AND status = 'pending')")
        .bind_refs(&[D1Type::Text(&email.id)])?.run().await?;
    Ok(())
}

#[derive(Deserialize)]
struct InboundJob {
    id: String,
    envelope_from: String,
    envelope_to: String,
    received_at: i64,
}

pub async fn run(env: &Env) -> AppResult<()> {
    let db = env.d1("DB")?;
    let settings = config::load_settings(&db).await?;
    let now = util::now_secs();
    if settings.r2_configured() {
        let jobs: Vec<InboundJob> = db.prepare("UPDATE inbound_jobs SET lease_until = ?1 + 600, attempts = attempts + 1 WHERE id IN (SELECT id FROM inbound_jobs WHERE available_at <= ?1 AND lease_until <= ?1 ORDER BY available_at LIMIT 1) RETURNING id, envelope_from, envelope_to, received_at")
            .bind_refs(&[D1Type::Real(now as f64)])?.all().await?.results()?;
        for job in jobs {
            let result = async {
                let mut response = r2::get_attachment(&settings, &r2::raw_key(&job.id), None)
                    .await?
                    .ok_or_else(|| {
                        crate::error::AppError::Internal("原始邮件尚未写入 R2".into())
                    })?;
                let raw = response.bytes().await?;
                inbound::process(
                    &settings,
                    &db,
                    &job.id,
                    raw,
                    &job.envelope_to,
                    &job.envelope_from,
                    job.received_at,
                )
                .await?;
                Ok::<(), crate::error::AppError>(())
            }
            .await;
            let error = result.err().map(|e| format!("{e:?}"));
            if let Some(e) = &error {
                console_error!("[mailflare] 恢复任务 {}: {e}", job.id);
            }
            // 已完成任务已被删除；未完成则指数退避，最多每小时一次，保留人工恢复机会。
            db.prepare("UPDATE inbound_jobs SET lease_until = 0, available_at = ?1 + min(3600, 60 * (1 << min(attempts, 6))), last_error = ?2 WHERE id = ?3")
                .bind_refs(&[D1Type::Real(util::now_secs() as f64),error.as_deref().map_or(D1Type::Null,D1Type::Text),D1Type::Text(&job.id)])?.run().await?;
        }
    }
    run_notifications(env).await?;
    db.prepare("DELETE FROM login_attempts WHERE reset_at < ?1")
        .bind_refs(&[D1Type::Real(now as f64)])?
        .run()
        .await?;
    Ok(())
}

#[derive(Deserialize)]
struct NotificationJob {
    email_id: String,
    channel: String,
    attempts: i64,
    expires_at: i64,
}

pub async fn run_notifications(env: &Env) -> AppResult<()> {
    let db = env.d1("DB")?;
    let settings = config::load_settings(&db).await?;
    let now = util::now_secs();
    // 领取时不排除过期任务，确保它们最终变成 failed 而非永久 pending。
    let jobs: Vec<NotificationJob> = db.prepare("UPDATE notification_jobs SET lease_until = ?1 + 300, attempts = attempts + 1 WHERE rowid IN (SELECT rowid FROM notification_jobs WHERE status = 'pending' AND available_at <= ?1 AND lease_until <= ?1 ORDER BY available_at LIMIT 10) RETURNING email_id, channel, attempts, expires_at")
        .bind_refs(&[D1Type::Real(now as f64)])?.all().await?.results()?;
    for job in jobs {
        let Some(email) = db::get_email(&db, &job.email_id).await? else {
            continue;
        };
        let enabled = if job.channel == "telegram" {
            settings.telegram_enabled
        } else {
            settings.bark_enabled
        };
        let code = crate::otp::extract(&email.subject, &email.text_body, &email.html_body);
        let status = if email.deleted_at.is_some() || !enabled {
            "skipped"
        } else if job.expires_at <= util::now_secs() || job.attempts > 3 {
            "failed"
        } else {
            if let Some(code) = &code {
                let push = notify::PushEmail {
                    sender: email.sender.clone(),
                    subject: email.subject.clone(),
                    code: code.clone(),
                };
                let result = notify::send_channel(
                    &notify::PushContext {
                        settings: &settings,
                        email: &push,
                    },
                    &job.channel,
                )
                .await;
                match result {
                    notify::ChannelResult::Ok => "ok",
                    notify::ChannelResult::Skipped => "skipped",
                    notify::ChannelResult::Failed(e) => {
                        console_error!(
                            "[mailflare] {} 推送失败 {}: {e}",
                            job.channel,
                            job.email_id
                        );
                        if job.attempts >= 3 {
                            "failed"
                        } else {
                            "pending"
                        }
                    }
                }
            } else {
                "skipped"
            }
        };
        db.prepare("UPDATE notification_jobs SET status = ?1, lease_until = 0, available_at = ?2 WHERE email_id = ?3 AND channel = ?4")
            .bind_refs(&[D1Type::Text(status),D1Type::Real((util::now_secs()+60*job.attempts) as f64),D1Type::Text(&job.email_id),D1Type::Text(&job.channel)])?.run().await?;
        db.prepare("UPDATE emails SET push_status = CASE WHEN EXISTS (SELECT 1 FROM notification_jobs WHERE email_id = ?1 AND status = 'pending') THEN 'pending' WHEN EXISTS (SELECT 1 FROM notification_jobs WHERE email_id = ?1 AND status = 'failed') THEN 'failed' WHEN EXISTS (SELECT 1 FROM notification_jobs WHERE email_id = ?1 AND status = 'ok') THEN 'ok' ELSE NULL END WHERE id = ?1")
            .bind_refs(&[D1Type::Text(&job.email_id)])?.run().await?;
        if (status == "ok" || status == "failed")
            && let Some(code) = code
        {
            notify::record_push_log(
                &db,
                &notify::PushLog {
                    at: util::now_secs(),
                    subject: email.subject,
                    code: code.value,
                    ok: status == "ok",
                },
            )
            .await;
        }
    }
    Ok(())
}
