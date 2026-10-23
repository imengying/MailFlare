# MailFlare ✉️

极简、高性能的**单用户**个人邮件管理系统。一个 Cloudflare Worker 同时承担
「收信」与「管理后台」两个角色：

- **收信**：Cloudflare Email Routing → Rust 解析（`mail-parser`，零拷贝）→ 附件写入 R2 → 元数据入库 D1 → `ctx.waitUntil()` 异步推送 Telegram / Bark
- **管理**：Rust 后端 API + 内嵌单页前端，密码 + Cloudflare Turnstile 登录，无状态 JWT（HMAC-SHA256）
- **存储**：D1 只存 2 张表（`configs` / `emails`），R2 存原始 `.eml` 与附件

```
                     ┌──────────────────────────────┐
   SMTP ──► Email    │  Worker: #[event(email)]      │
            Routing ─┤   mail-parser（零拷贝解析）   │──► R2  raw/*.eml
                     │   buffer_unordered 并发写 R2  │──► R2  att/{id}/{n}
                     │   D1 INSERT OR IGNORE（幂等） │──► D1  emails
                     └───────────────┬──────────────┘
                                     │ ctx.wait_until()
                                     ▼
                          Telegram Bot  ‖  Bark（并发）

   Browser ──► /api/*  ──► Worker: #[event(fetch)] ──► D1 / R2
                     └──► 其它路径 ──► [assets] 单页前端
```

---

## 1. 目录结构

```
MailFlare/
├── Cargo.toml                 # Rust 2024 + workers-rs / mail-parser / jsonwebtoken
├── wrangler.toml              # D1 / R2 / vars / [assets] 绑定
├── schema.sql                 # D1 建表 + 索引
├── DEPLOY.md                  # 网页控制台部署指南（连 GitHub）
├── package.json               # 前端工具链（bun + TypeScript 7）
├── tsconfig.json              # 严格模式 TS 配置
├── rust-toolchain.toml        # 固定 stable + wasm32 target
├── bunfig.toml                # bun 精确版本安装 + 缓存目录
├── .cargo/config.toml         # target-dir 指向 .cache/
│
├── src/                       # ── 后端（Rust）──
│   ├── lib.rs                 # fetch / email 两个事件入口
│   ├── api.rs                 # HTTP 路由与 handler
│   ├── auth.rs                # Turnstile 校验 + JWT 签发/校验
│   ├── config.rs              # 环境变量 + configs 表
│   ├── db.rs                  # D1 数据访问
│   ├── inbound.rs             # 收信流水线（R2 → D1 → 推送）
│   ├── mail.rs                # mail-parser 解析核心
│   ├── notify.rs              # Telegram / Bark 并发推送
│   ├── r2.rs                  # R2 对象读写
│   ├── util.rs                # 时间 / ID / 截断 / 常量时间比较
│   └── tests.rs               # 单元测试（中文编码、CID 重写、截断…）
│
├── web/                       # ── 前端（TypeScript）──
│   ├── index.template.html    # HTML 模板 + CSS（含脚本注入标记）
│   └── src/
│       ├── app.ts             # 主入口：状态、事件、轮询
│       ├── api-types.ts       # 与 Rust 结构体一一对应的 DTO
│       ├── api-client.ts      # fetch 封装、401 处理、信封解包
│       ├── state.ts           # 应用状态
│       ├── list-view.ts       # 列表渲染
│       ├── detail-view.ts     # 详情渲染
│       ├── settings-view.ts   # 通知设置抽屉
│       ├── mail-render.ts     # 邮件 HTML 安全渲染（sandbox + CSP）
│       ├── turnstile.ts       # Turnstile 挂件
│       ├── dom.ts             # DOM 与格式化工具
│       └── ui.ts              # Toast / 空状态 / 骨架屏
│
├── scripts/
│   ├── build.sh               # 云端构建入口（装 Rust → 建前端 → 编 wasm）
│   ├── build.ts               # bun 构建：TS → 内联进 public/index.html
│   └── check-render.ts        # 渲染安全自检
└── public/
    └── index.html             # 构建产物（.gitignore 忽略，勿手改）
```

---

## 2. 配置清单

### 2.1 `Cargo.toml` 要点

| 依赖 | 作用 | 注意事项 |
|---|---|---|
| `worker` 0.8.7 | Workers SDK | 必须开 `features = ["d1"]` |
| `mail-parser` 0.11.9 | MIME 解析 | 必须开 `features = ["full_encoding"]`，否则 GBK/GB2312/Big5 会被当成 UTF-8 解成乱码 |
| `hmac` 0.13 + `sha2` 0.11 + `base64` 0.23 | HS256 JWT | 见下节，**不使用 `jsonwebtoken`** |
| `sha2` | 密码摘要 | 先 SHA-256 再常量时间比较，避免长度侧信道 |
| `getrandom` 0.4 | 随机数 | wasm32 上必须开 `features = ["wasm_js"]`（0.4 把 `js` 改名了） |

### 2.1.1 为什么不用 `jsonwebtoken`

我们只需要 **HS256 对称签名**，而 `jsonwebtoken` 会拖进
`rsa` / `p256` / `p384` / `ed25519-dalek` / `curve25519-dalek` 等一整套
非对称加密实现（只为支持我们根本不用的 RS256/ES256/EdDSA），并且内部锁定
`getrandom ^0.2`——在 wasm32 上还得额外为它开 `js` feature。

因此改为 30 行左右的 `/src/jwt.rs`，依赖只有：

| crate | 用途 |
|---|---|
| `hmac` 0.13 | HMAC-SHA256（依赖 `digest 0.11`，与 `sha2 0.11` 配套） |
| `sha2` 0.11 | SHA-256（HMAC 底层 + 密码摘要） |
| `base64` 0.23 | `URL_SAFE_NO_PAD` 编解码 |

**收益**：wasm 从 1.6MB 降到 **1016KB**（gzip 627KB → 434KB），
依赖树里不再有任何非对称加密 crate。

**安全性**：手写实现反而更安全——头部 `alg` 写死为 `HS256`，
不存在算法协商，天然免疫 `alg=none` 与 RS256→HS256 混淆攻击
（两者都有对应测试）。签名用常量时间比较。已用 Python 标准库
（`hmac` + `hashlib`）做**双向互操作验证**。

> ⚠️ 不要直接 `cargo add hmac`：`hmac 0.12` 依赖 `digest 0.10`，与
> `sha2 0.11` 的 `digest 0.11` 不兼容，会同时编进两份 digest。
> 必须用 `hmac 0.13`。

> ⚠️ **不要**在 `[profile.release]` 里加 `strip = true`。worker-build 的 wasm-bindgen 步骤
> 需要 wasm 内的符号信息来生成 catch wrapper，剥离后会报
> `externref table required for catch wrappers` 并中断构建。

### 2.1.2 前端依赖（`package.json`，用 bun 管理）

| 依赖 | 作用 |
|---|---|
| `typescript` 7.0.2 | 原生 Go 编译器版本，类型检查用 |
| `@types/bun` 1.4.3 | 构建脚本里的 `Bun.build` / `Bun.write` 类型 |

前端**没有运行时依赖**：不用框架、不用打包器（`Bun.build` 内置），
产物是单个 IIFE 内联进 HTML。`bun.lock` 需提交以保证依赖可复现。

```bash
bun install          # 安装
bun run check        # tsc --noEmit，严格模式零错误
bun run build        # 打包并内联到 public/index.html
bun run dev          # 监听 web/src，改动自动重建
```

### 2.2 `wrangler.toml` 要点

```toml
[[d1_databases]]
binding = "DB"

[[r2_buckets]]
binding = "R2"

[assets]
directory = "./public"
binding = "ASSETS"
run_worker_first = ["/api/*"]                 # API 必须先到 Worker
not_found_handling = "single-page-application" # 前端路由回落 index.html
```

`run_worker_first` 是关键：没有它，`not_found_handling` 会把不存在的
`/api/...` 也回成 `index.html`，前端拿到的就不是 JSON 了。

### 2.3 环境变量

敏感项用 `wrangler secret put`，其余可写在 `[vars]`：

| 变量 | 必填 | 说明 |
|---|---|---|
| `ADMIN_PASSWORD` | ✅（secret） | 管理员密码 |
| `JWT_SECRET` | ✅（secret） | HMAC 密钥，建议 `openssl rand -base64 48` |
| `TURNSTILE_SITE_KEY` | 建议 | Turnstile 公开 site key |
| `TURNSTILE_SECRET` | 建议（secret） | 留空则**跳过**人机验证（仅限本地开发） |
| `TURNSTILE_HOSTNAME` | 可选 | 限制 token 只能在指定 hostname 使用 |
| `PUBLIC_URL` | 可选 | 推送消息里「查看详情」链接的站点地址 |
| `ALLOWED_ORIGIN` | 可选 | 前后端分离部署时的 CORS 白名单 |
| `JWT_TTL_SECS` | 可选 | 默认 `604800`（7 天） |
| `STORE_RAW_EML` | 可选 | `0` 关闭原始 `.eml` 保存 |
| `MAX_BODY_BYTES` | 可选 | 正文入库上限，默认 256KB（超出截断） |
| `MAX_MESSAGE_BYTES` | 可选 | 单封邮件整体上限，默认 20MB（超出 SMTP 拒收） |
| `MAX_ATTACHMENT_BYTES` | 可选 | 单附件转存上限，默认 10MB |
| `R2_CONCURRENCY` | 可选 | 附件并发上传度，默认 4 |
| `PAGE_SIZE_DEFAULT` / `PAGE_SIZE_MAX` | 可选 | 分页大小，默认 20 / 100 |

---

## 3. 部署

### 推荐：Cloudflare 网页控制台 + GitHub（无需本地环境）

完整步骤见 **[DEPLOY.md](./DEPLOY.md)**，概要：

1. 控制台创建 D1 数据库 `mailflare`，在 Console 里执行 `schema.sql`
2. 控制台创建 R2 桶 `mailflare-mail`
3. Workers → Create → **Connect to Git** 选本仓库
4. Bindings 里加 `DB`（D1）和 `R2`（R2 bucket）
5. Variables and Secrets 里加 `ADMIN_PASSWORD` / `JWT_SECRET` / `TURNSTILE_SECRET`（均选 **Secret**）
6. Deploy
7. Email Routing → 路由规则 → Send to Worker → 选本 Worker

云端构建配置：

| 项 | 值 |
|---|---|
| Build command | `bun run build` |
| Deploy command | `bunx wrangler deploy` |
| Build variable | `BUN_VERSION=1.4.3` |

`bun run build` 会执行 `scripts/build.sh`。**Cloudflare 构建镜像没有预装
Rust 工具链，也没有 pkg-config**，所以脚本自己负责：

- 安装 Rust（rustup 官方脚本，minimal profile）
- 添加 `wasm32-unknown-unknown` target
- 显式设置 `OPENSSL_INCLUDE_DIR` / `OPENSSL_LIB_DIR`，绕开
  `openssl-sys` 对 pkg-config 的探测（缺失会 panic）
- 安装 `worker-build 0.8.7`
- 构建前端（bun）→ 编译 wasm（worker-build）

脚本是**幂等**的：一次部署里可能被调用两次（Dashboard 的 Build command
与 `wrangler.toml` 的 `[build].command`），已构建且源文件未变时直接跳过。

首次部署约 5–7 分钟，之后命中缓存约 2 分钟。

### 备选：本地 wrangler

```bash
rustup target add wasm32-unknown-unknown
cargo install worker-build --version 0.8.7 --locked
bun install

wrangler d1 create mailflare        # 把 database_id 填进 wrangler.toml
wrangler r2 bucket create mailflare-mail
wrangler d1 execute mailflare --remote --file=./schema.sql

wrangler secret put ADMIN_PASSWORD
wrangler secret put JWT_SECRET
wrangler secret put TURNSTILE_SECRET

wrangler deploy
```

### 本地开发

```bash
bun run dev                    # 监听前端 TS，改动自动重建
wrangler dev --local           # 启动本地 Worker
wrangler d1 execute mailflare --local --file=./schema.sql
```

本地测试收信可用 Email Routing 的 "Send test email"，
或直接调用 `POST /cdn-cgi/handler/email`（需 `wrangler` 支持）。

---

## 4. API

所有成功响应为 `{"ok":true,"data":...}`，错误为 `{"ok":false,"error":"..."}`。

| 方法 | 路径 | 鉴权 | 说明 |
|---|---|---|---|
| `GET` | `/api/health` | — | 健康检查 |
| `GET` | `/api/auth/config` | — | 返回 Turnstile site key |
| `POST` | `/api/auth/login` | — | `{password, turnstile_token}` → `{token, expires_in}` |
| `POST` | `/api/auth/logout` | — | 占位（无状态 JWT） |
| `GET` | `/api/auth/session` | Bearer | 校验 token |
| `GET` | `/api/emails` | Bearer | `?filter=unread&limit=20&cursor=...` |
| `GET` | `/api/emails/:id` | Bearer | 详情；自动标记已读；返回附件 URL 与 `raw_url` |
| `DELETE` | `/api/emails/:id` | Bearer | 删 D1 记录 + 联动删 R2 对象 |
| `POST` | `/api/emails/:id/read` | Bearer | `{is_read:false}` 可标回未读 |
| `POST` | `/api/emails/read-all` | Bearer | 全部标为已读，返回受影响行数 |
| `GET` | `/api/emails/:id/download` | Bearer / 票据 | 下载原始 `.eml` |
| `GET` | `/api/attachments/:id` | Bearer / 票据 | 流式下载附件，`:id` = `{email_id}_{index}` |
| `GET` | `/api/settings` | Bearer | 推送配置（Token 脱敏） |
| `PUT` | `/api/settings` | Bearer | 更新推送配置 |

### 关于两种凭证

- **Bearer JWT**（`scp=api`）：常规 API 调用。
- **下载票据**（`scp=dl`，10 分钟）：只能访问 `sub` 对应的那一封邮件的附件。
  这是必需的——`<img src="...">` 无法携带 `Authorization` 头，而把长期
  token 拼进 URL 又会泄露。

---

## 5. 关键实现说明

### 5.1 内存与性能（WASM 128MB 上限）

1. **零拷贝解析**：`MessageParser::parse(&raw)` 返回的 `Message<'x>` 全程借用
   `raw`，header 字段都是原始缓冲区的切片；只有真正要落库的字段才 `to_owned()`。
2. **读之前先判大小**：`message.raw_size()` 是同步的，超限直接 `set_reject`，
   数据根本不进 WASM 堆。
3. **正文截断**：`truncate_utf8` 按字节上限截断并回退到 UTF-8 字符边界，
   就地 `truncate()`，不产生新分配。
4. **附件零额外拷贝**：`part.contents()` 直接给出解码后的 `&[u8]`，
   `to_vec()` 一次后即 move 进 `Bucket::put`；原始 `.eml` 的 `Vec<u8>` 也是 move。
5. **不存原文时尽早 drop**：`STORE_RAW_EML=0` 时立刻释放那块最多 20MB 的缓冲区。
6. **附件并发**：`buffer_unordered(R2_CONCURRENCY)` 既不是串行累加延迟，
   也不会一次开满子请求。
7. **下载不经 WASM**：附件用 `ResponseBody::Stream(R2 body)` 直接交给运行时转发，
   字节不经过 Rust 堆。
8. **列表只查摘要**：`substr(..., 1, 240)` 在 SQLite 里按字符截取，
   不会把几百 KB 的正文搬进 Worker。
9. **一次 RTT 拿列表 + 未读数**：两个查询合并成一次 `db.batch()`。

### 5.2 中文编码

`mail-parser` 开启 `full_encoding` 后会调用 `encoding_rs`，因此：

- RFC 2047 编码的标题（`=?GBK?B?...?=`、`=?UTF-8?B?...?=`）正确解码
- `charset=gb2312` / `gbk` / `gb18030` / `big5` 的正文正确解码
- 单元测试 `parses_gbk_body_and_mime_encoded_subject` 覆盖了这条路径

### 5.3 安全设计

| 风险 | 处理 |
|---|---|
| 密码爆破 | Turnstile + 连续 5 次失败锁定 5 分钟（状态存 `configs`） |
| 密码时序攻击 | SHA-256 摘要后常量时间比较 |
| 邮件 HTML XSS | 前端用 `sandbox="allow-same-origin"`（**不含** `allow-scripts`）的 iframe 渲染 `srcdoc` |
| 附件 XSS | 仅 `image/*` 与 `application/pdf` 允许 `inline`，其余强制 `attachment` + `application/octet-stream`；inline 附件额外带 `CSP: default-src 'none'; sandbox` |
| MIME 嗅探 | 所有响应带 `x-content-type-options: nosniff` |
| 越权读附件 | 先查 D1 校验「该附件属于这封邮件」，再查票据的 `sub` |
| 路径穿越 | `parse_attachment_id` 严格限定 `[A-Za-z0-9-]+_[0-9]+` |
| 缓存泄露 | 附件 `private, max-age=600`；API 一律 `no-store` |
| 重复投递 | `emails.message_id` 部分唯一索引 + `INSERT OR IGNORE`；命中后清理刚写入的 R2 对象 |

### 5.4 幂等与失败语义

| 场景 | 行为 |
|---|---|
| 同一 `Message-ID` 重复投递 | 静默成功，不重复入库，清理孤儿 R2 对象 |
| 单个附件超限 / 写 R2 失败 | 邮件照常入库，该附件标 `stored=false`，前端显示「未保存」 |
| 原始 `.eml` 写失败 | 只记日志，不影响邮件 |
| 解析失败 | `set_reject` 让发信方感知 |
| 推送失败 | 只记日志（`console_error`），绝不影响收信 |
| 邮件超大 / 解析失败 / D1·R2 异常 | **推送告警到 TG/Bark**（`waitUntil` 异步，不阻塞 SMTP），避免静默丢信 |
| 重复投递 | 不告警（SMTP 重试是预期行为） |
| 删除邮件 | 先删 D1 再删 R2（避免出现「列表有、点开 404」） |

---

## 6. 前端

前端是 **TypeScript + bun 构建**，源码在 `web/src/`，产物内联进 `public/index.html`
（构建产物已 `.gitignore`，请勿手改）。无框架、无运行时依赖：

```bash
bun run check    # 严格模式类型检查
bun run build    # 打包 → public/index.html
bun run dev      # 监听重建
```


功能：
- 左侧邮件列表（游标分页 + 本地搜索 + 未读角标 + 全部已读）
- 右侧详情（HTML/纯文本切换、附件下载、`.eml` 下载、标为未读、删除）
- 设置抽屉（Telegram / Bark，Token 输入框留空表示不修改）
- 亮/暗色自动适配；移动端抽屉式侧栏
- 页面可见时每 60 秒静默刷新（保留选中项与滚动位置）

### 6.1 前后端契约

TypeScript 只能保证前端内部一致，**跨语言契约靠人工同步**：
`web/src/api-types.ts` 里的字段必须与 `src/db.rs` / `src/api.rs` 的
`Serialize` 结构体一一对应。改动任一侧都要同步另一侧。

风险最高的是 `EmailDetailResponse`：后端用 `#[serde(flatten)]` 把
`EmailDetail` 的字段平铺到顶层，所以 TS 侧写成交叉类型而不是嵌套对象。

### 6.2 安全渲染

邮件 HTML 是**完全不可信**的第三方内容，渲染时有三道独立防线：

1. `sandbox="allow-same-origin allow-popups allow-popups-to-escape-sandbox"` 的 iframe
   —— **不含** `allow-scripts`（防 XSS）、**不含** `allow-top-navigation`（防页面被劫持）、
   **不含** `allow-forms`（防钓鱼表单）。`allow-popups*` 是必需的，否则邮件里的
   链接点了没反应。配合给所有 `<a>` 注入 `rel="noopener noreferrer"`，
   即使新标签页脱离了沙箱也拿不到 `opener` 句柄。
2. `srcdoc` 内嵌 CSP：`default-src 'none'`、`form-action 'none'`
3. 默认 `img-src 'self' data:`，拦掉远程图片（防追踪像素泄露 IP），
   用户点「显示图片」才放开

列表预览全部走 `textContent` 赋值，不碰 `innerHTML`。取 HTML 纯文本兜底时用
`DOMParser`（惰性解析，不发起请求、不执行脚本）而不是 `div.innerHTML`。

---

## 7. 测试

```bash
# 后端
cargo test                                      # 41 个测试
cargo clippy --all-targets --target wasm32-unknown-unknown   # 零警告
worker-build --release                          # wasm ~1016KB（gzip 后 ~434KB）

# 前端
bun run check                                   # tsc 严格模式零错误
bun run check:render                            # 渲染安全自检（35 项断言）
bun run build                                   # 内联产物 ~17KB JS
```

覆盖：
- **邮件解析**：GBK 正文 + RFC2047 标题、MIME 附件、内联 `cid`
- **存储与工具**：UTF-8 边界截断、零拷贝（短文本不重分配）、常量时间比较
- **输入校验**：附件 ID 严格解析、inline 白名单、`Content-Disposition` 双格式
- **渲染安全**：CID 单遍重写、HTML 去标签
- **JWT**（11 项）：往返、篡改载荷、错误密钥、`alg=none`、
  RS256 混淆、过期与宽容期、畸形输入、非法 base64、
  与 Python 标准库的**双向互操作验证**

---

## 8. 已知取舍

- **单用户**：JWT 里没有用户维度；多用户需要改 `emails` 加 owner 列与行级过滤。
- **搜索**：列表页是前端在已加载的页内过滤，不查 D1。要全库搜索可加
  `emails_fts` 虚表（SQLite FTS5，D1 支持）。
- **限流状态存 D1**：并发爆破时可能有竞态（读-改-写），但对单用户场景足够；
  要严格可以把 `LoginGuard` 搬到 Durable Object。
- **附件下载票据不可撤销**：10 分钟自然过期。需要立即失效就得引入
  token 版本号。
- **原始 `.eml` 存 R2 时是整份 move 进 `put`**：Cloudflare R2 单次 put 上限
  约 315MB，而 Email Routing 本身限制在 25MB 级别，不会触顶。
