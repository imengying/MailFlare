/**
 * DOM 与格式化小工具。
 *
 * 所有 DOM 查询都带非空断言的理由：这些元素全部写死在 `index.html` 里，
 * 属于「编译期已知的模板契约」。用 `el()` 把「找不到就抛」集中在
 * 一处，比在几十个调用点重复 `?.` 判断更不容易漏。
 */

/** 按 CSS 选择器取元素，找不到直接抛错（模板被改动时立刻暴露，而不是静默失效）。 */
export function el<T extends Element>(selector: string): T {
  const node = document.querySelector<T>(selector);
  if (!node) {
    throw new Error(`模板缺少元素：${selector}`);
  }
  return node;
}

/**
 * `create()` 接受的属性类型。
 *
 * 把 `style` 单独放宽成字符串：DOM 的 `element.style` 是
 * `CSSStyleDeclaration` 对象，直接赋值字符串会类型报错，
 * 但写内联样式时字符串显然更好用。
 */
export type CreateProps<K extends keyof HTMLElementTagNameMap> = Omit<
  Partial<HTMLElementTagNameMap[K]>,
  "style"
> & { style?: string };

/**
 * 创建元素并批量赋值属性。
 *
 * 全程走 DOM 属性赋值、不碰 `innerHTML`，因此天然免疫 HTML 注入。
 */
export function create<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: CreateProps<K> | Record<string, never> = {},
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  const { style, ...rest } = props as CreateProps<K> & { style?: string };
  Object.assign(node, rest);
  if (style !== undefined) node.style.cssText = style;
  return node;
}

/**
 * 把毫秒时间戳格式化成列表里的紧凑写法。
 *
 * 规则：今天只显示时分；今年内显示月日；跨年才显示完整日期。
 */
export function formatTime(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return "";

  const now = new Date();
  const pad = (n: number): string => String(n).padStart(2, "0");

  if (d.toDateString() === now.toDateString()) {
    return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }
  if (d.getFullYear() === now.getFullYear()) {
    return `${d.getMonth() + 1}月${d.getDate()}日`;
  }
  return `${d.getFullYear()}/${pad(d.getMonth() + 1)}/${pad(d.getDate())}`;
}

/** 字节数转人类可读体积。 */
export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/** 是否处于移动端断点（与 CSS 里的 860px 保持一致）。 */
export function isMobile(): boolean {
  return window.matchMedia("(max-width: 860px)").matches;
}

/** 当前是否暗色主题（用于 Turnstile 挂件配色）。 */
export function prefersDark(): boolean {
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}
