-- ============================================================================
-- MailFlare · 初始表结构
--
-- 仅供全新数据库初始化。部署脚本不会在已有业务表的数据库重放本文件。
-- 已有数据库按 README 手动升级；保留原来的 d1_migrations 记录。
-- ============================================================================

-- 动态配置表：后台设置（Turnstile、推送、收信过滤与限额、分页等）
-- 以 key/value 形式存放；登录失败计数使用独立的 login_attempts 表。
-- settings 的值是一条 JSON，这样新增配置项时无需再做数据库迁移。
CREATE TABLE IF NOT EXISTS configs (
  key        TEXT PRIMARY KEY,
  value      TEXT,
  updated_at INTEGER NOT NULL DEFAULT 0
);

-- 邮件表。正文（纯文本 + HTML）与附件元数据全部在这里，
-- R2 存附件字节及完整原文。
CREATE TABLE IF NOT EXISTS emails (
  id          TEXT PRIMARY KEY,
  message_id  TEXT,
  sender      TEXT,
  recipient   TEXT,
  subject     TEXT,
  text_body   TEXT,
  html_body   TEXT,
  attachments TEXT,
  is_read     INTEGER DEFAULT 0,
  created_at  INTEGER NOT NULL DEFAULT 0,
  push_status TEXT,
  raw_key     TEXT,
  storage_status TEXT NOT NULL DEFAULT 'complete',
  deleted_at  INTEGER,
  otp_code    TEXT
);

-- 列表页永远按时间倒序 + 未读过滤，建立覆盖索引避免全表扫描。
CREATE INDEX IF NOT EXISTS idx_emails_created_at ON emails (created_at DESC);
CREATE INDEX IF NOT EXISTS idx_emails_unread     ON emails (is_read, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_emails_folder_cursor ON emails (deleted_at, created_at DESC, id DESC);

-- 先登记任务再存原文；即使确认原文后的 D1 更新失败，定时任务仍能恢复。
CREATE TABLE IF NOT EXISTS inbound_jobs (
  id TEXT PRIMARY KEY,
  envelope_from TEXT NOT NULL,
  envelope_to TEXT NOT NULL,
  received_at INTEGER NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  available_at INTEGER NOT NULL,
  lease_until INTEGER NOT NULL DEFAULT 0,
  last_error TEXT
);
CREATE INDEX IF NOT EXISTS idx_inbound_due ON inbound_jobs (available_at, lease_until);

CREATE TABLE IF NOT EXISTS notification_jobs (
  email_id TEXT NOT NULL REFERENCES emails(id) ON DELETE CASCADE,
  channel TEXT NOT NULL CHECK (channel IN ('telegram', 'bark')),
  status TEXT NOT NULL DEFAULT 'pending',
  attempts INTEGER NOT NULL DEFAULT 0,
  available_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  lease_until INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (email_id, channel)
);
CREATE INDEX IF NOT EXISTS idx_notification_due ON notification_jobs (status, available_at, lease_until);

-- 按来源计数，单条 UPSERT 原子占用一次尝试，不共享管理员全局锁。
CREATE TABLE IF NOT EXISTS login_attempts (
  source TEXT PRIMARY KEY,
  attempts INTEGER NOT NULL,
  reset_at INTEGER NOT NULL
);
-- 同一封邮件可能分别投递给多个别名；只在同一信封收件地址内去重。
CREATE UNIQUE INDEX IF NOT EXISTS idx_emails_message_recipient
  ON emails (message_id, recipient)
  WHERE message_id IS NOT NULL AND message_id <> '';
