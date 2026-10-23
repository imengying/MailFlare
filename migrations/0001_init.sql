-- ============================================================================
-- MailFlare · 初始表结构
--
-- 由 `wrangler d1 migrations apply DB --remote` 自动执行，
-- 不需要手工到控制台粘 SQL。
-- ============================================================================

-- 动态配置表：后台设置（Turnstile、推送、收信过滤与限额、分页等）
-- 与登录失败计数全部以 key/value 形式存放。
-- settings 的值是一条 JSON，这样新增配置项时无需再做数据库迁移。
CREATE TABLE IF NOT EXISTS configs (
  key        TEXT PRIMARY KEY,
  value      TEXT,
  updated_at INTEGER NOT NULL DEFAULT 0
);

-- 邮件表。正文（纯文本 + HTML）与附件元数据全部在这里，
-- R2 只存附件字节。
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
