-- 原文恢复、回收站、通知重试、登录限流与按收件地址去重。
ALTER TABLE emails ADD COLUMN push_status TEXT;
ALTER TABLE emails ADD COLUMN raw_key TEXT;
ALTER TABLE emails ADD COLUMN storage_status TEXT NOT NULL DEFAULT 'complete';
ALTER TABLE emails ADD COLUMN deleted_at INTEGER;
CREATE INDEX idx_emails_folder_cursor ON emails (deleted_at, created_at DESC, id DESC);

-- 先登记任务再存原文；即使确认原文后的 D1 更新失败，定时任务仍能恢复。
CREATE TABLE inbound_jobs (
  id TEXT PRIMARY KEY,
  envelope_from TEXT NOT NULL,
  envelope_to TEXT NOT NULL,
  received_at INTEGER NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  available_at INTEGER NOT NULL,
  lease_until INTEGER NOT NULL DEFAULT 0,
  last_error TEXT
);
CREATE INDEX idx_inbound_due ON inbound_jobs (available_at, lease_until);

CREATE TABLE notification_jobs (
  email_id TEXT NOT NULL REFERENCES emails(id) ON DELETE CASCADE,
  channel TEXT NOT NULL CHECK (channel IN ('telegram', 'bark')),
  status TEXT NOT NULL DEFAULT 'pending',
  attempts INTEGER NOT NULL DEFAULT 0,
  available_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  lease_until INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (email_id, channel)
);
CREATE INDEX idx_notification_due ON notification_jobs (status, available_at, lease_until);

-- 按来源计数，单条 UPSERT 原子占用一次尝试，不共享管理员全局锁。
CREATE TABLE login_attempts (
  source TEXT PRIMARY KEY,
  attempts INTEGER NOT NULL,
  reset_at INTEGER NOT NULL
);
-- 同一封邮件可能分别投递给多个别名；只在同一信封收件地址内去重。
-- 先建立新索引再移除旧索引，保留所有历史邮件与对象路径。
CREATE UNIQUE INDEX idx_emails_message_recipient
  ON emails (message_id, recipient)
  WHERE message_id IS NOT NULL AND message_id <> '';

DROP INDEX IF EXISTS idx_emails_message_id;
