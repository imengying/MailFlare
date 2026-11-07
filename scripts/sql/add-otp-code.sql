-- 适用于已完成原 0002 升级的数据库；在发布新版 Worker 前执行一次。
-- 可先执行 PRAGMA table_info(emails); 若已有 otp_code 列则跳过本文件。
-- 保留旧邮件数据；旧邮件的列表验证码为 NULL，详情仍可实时提取。
ALTER TABLE emails ADD COLUMN otp_code TEXT;
