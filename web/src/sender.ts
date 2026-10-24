/** 发件人显示名称与纯文本头像。 */

/** 优先使用邮件显示名，未命名时保留邮箱地址。 */
export function senderLabel(sender: string): string {
  if (!sender) return "(未知发件人)";
  const match = /^(.*)<[^>]+>\s*$/.exec(sender);
  return match?.[1]?.trim().replace(/^["']|["']$/g, "") || sender;
}

/** 取第一个字母或数字；DOM 只用 textContent，不解析发件人 HTML。 */
export function senderInitial(sender: string): string {
  for (const char of senderLabel(sender).trim()) {
    if (/[\p{L}\p{N}]/u.test(char)) return char.toLocaleUpperCase();
  }
  return "@";
}

/** 同一发件人的头像颜色保持稳定，不随排序或筛选变化。 */
export function senderTone(sender: string): number {
  let hash = 0;
  for (const char of senderLabel(sender)) hash = (hash * 31 + (char.codePointAt(0) ?? 0)) >>> 0;
  return hash % 4;
}
