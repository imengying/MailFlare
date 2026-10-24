/**
 * 构建脚本（bun 原生执行，无需额外打包器）。
 *
 * 做的事：
 * 1. 用 `Bun.build` 把 web/src/app.ts 打包成单个 IIFE（浏览器直接可用）
 * 2. 把产物内联进 public/index.html 的 `<script>` 位置
 *
 * 为什么内联而不是产出独立 .js：
 * 整个前端只有一个 Worker 静态资源入口，内联能省掉一次请求往返，
 * 也不用在 wrangler.toml 里额外声明资源路由。
 */

import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";

const ROOT = resolve(import.meta.dir, "..");
const ENTRY = resolve(ROOT, "web/src/app.ts");
const TEMPLATE = resolve(ROOT, "web/index.template.html");
const OUTPUT = resolve(ROOT, "public/index.html");

const MARKER_START = "<!-- APP_SCRIPT_START -->";
const MARKER_END = "<!-- APP_SCRIPT_END -->";

/** 打包脚本并返回压缩后的 JS 源码。 */
async function bundle(): Promise<string> {
  const result = await Bun.build({
    entrypoints: [ENTRY],
    target: "browser",
    format: "iife",
    minify: true,
    // 生产构建不需要 sourcemap：产物要内联进 HTML，带上会显著增大体积。
    // 需要调试时用 `bun run dev`（下面会保留 sourcemap 写入独立文件）。
    sourcemap: "none",
  });

  if (!result.success) {
    for (const log of result.logs) {
      console.error(log);
    }
    throw new Error("打包失败");
  }

  const first = result.outputs[0];
  if (!first) throw new Error("打包没有产出文件");
  return await first.text();
}

/** 把 JS 内联进模板的标记位置。 */
async function inline(js: string): Promise<void> {
  const template = await readFile(TEMPLATE, "utf8");

  const start = template.indexOf(MARKER_START);
  const end = template.indexOf(MARKER_END);
  if (start < 0 || end < 0 || end < start) {
    throw new Error(
      `模板里找不到 ${MARKER_START} / ${MARKER_END} 标记，无法注入脚本`,
    );
  }

  // `</script>` 不能出现在内联脚本里，否则会提前结束标签。转义成 `<\/script>`。
  const safe = js.replace(/<\/script>/gi, "<\\/script>");

  const output =
    template.slice(0, start + MARKER_START.length) +
    `\n<script>\n${safe}\n</script>\n` +
    template.slice(end);

  // `public/` 是被 gitignore 的构建输出目录，干净克隆里根本不存在
  // （git 不跟踪空目录），所以写文件前必须先建目录。
  await mkdir(dirname(OUTPUT), { recursive: true });
  await writeFile(OUTPUT, output, "utf8");
}

async function main(): Promise<void> {
  const watch = process.argv.includes("--watch");

  const build = async (): Promise<void> => {
    const started = performance.now();
    const js = await bundle();
    await inline(js);
    const ms = (performance.now() - started).toFixed(0);
    const kb = (Buffer.byteLength(js) / 1024).toFixed(1);
    console.log(`✔ public/index.html · ${kb} KB JS · ${ms}ms`);

    if (watch) {
      console.log("👀 监听 web/src，改动后自动重建…");
    }
  };

  await build();

  if (watch) {
    const { watch: watchFs } = await import("node:fs");
    watchFs(resolve(ROOT, "web/src"), { recursive: true }, () => {
      void build().catch((error: unknown) => {
        console.error("重建失败：", error);
      });
    });
  }
}

await main();
