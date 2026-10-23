-- ============================================================================
-- MailFlare · D1 初始化脚本
-- 执行：wrangler d1 execute mailflare --file=./schema.sql --remote
-- 本地：wrangler d1 execute mailflare --file=./schema.sql --local
-- ============================================================================

-- 动态配置表：通知设置、登录失败计数等全部以 key/value 形式存放，
-- 这样新增配置项时无需再做数据库迁移。
CREATE TABLE IF NOT EXISTS configs (
  key        TEXT PRIMARY KEY,
  value      TEXT,
  updated_at INTEGER NOT NULL DEFAULT 0
);

-- 邮件元数据表。正文只存裁剪后的文本/HTML，原始 .eml 与附件放 R2。
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
  created_at  INTEGER NOT NULL DEFAULT 0
);

-- 列表页永远按时间倒序 + 未读过滤，建立覆盖索引避免全表扫描。
CREATE INDEX IF NOT EXISTS idx_emails_created_at ON emails (created_at DESC);
CREATE INDEX IF NOT EXISTS idx_emails_unread     ON emails (is_read, created_at DESC);

-- 幂等去重：同一封邮件被重复投递（SMTP 重试）时只入库一次。
-- 使用部分索引，允许大量 message_id 为 NULL 的历史数据共存。
CREATE UNIQUE INDEX IF NOT EXISTS idx_emails_message_id
  ON emails (message_id)
  WHERE message_id IS NOT NULL AND message_id <> '';
