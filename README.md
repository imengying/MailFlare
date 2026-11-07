# MailFlare ✉️

极简的**单用户**个人邮件系统。一个 Cloudflare Worker 同时收信和当后台：

- **收信**：Email Routing → 保留原始 EML → 解析与附件转存 → D1 入库 → 持久化任务推送 Telegram / Bark
- **管理**：Rust API + 内嵌单页前端，用户名 + 密码 + Turnstile 登录，无状态 JWT（HS256）
- **存储**：邮件正文与元数据进 D1（`configs` / `emails`），原始 EML 与附件字节进 R2（S3 API）

```
   SMTP ──► Email Routing ──► #[event(email)]
                                ├─► R2   mail/{id}/original.eml + mail/{id}/{n}
                                ├─► D1   emails
                                └─► D1 任务 + waitUntil / Cron ──► Telegram / Bark

   Browser ──► /api/*  ──► #[event(fetch)] ──► D1 / R2
           └─► 其它路径 ──► [assets] 单页前端
```

---

## 部署

全部在 Cloudflare 网页完成，不需要本地装 Rust 或 wrangler。

### 1. 建 R2 桶并生成 API 令牌

**R2 Object Storage → Create bucket**，名字随你取。

然后在 **R2 → Manage R2 API Tokens → Create API Token** 生成一对
Access Key ID / Secret Access Key（权限选 **Object Read & Write**，
资源范围选这个桶）。这两项连同**账号 ID**、**桶名**，等会儿在网页设置里填。

> 不需要在 `wrangler.toml` 里声明 `[[r2_buckets]]`，也不需要去
> Settings → Bindings 绑定。附件走的是 R2 的 S3 API，四项配置都存在 D1 里，
> 随时可改，改完即时生效。

### 2. 连 GitHub 部署

**Workers & Pages → Create → Workers → Connect to Git**，选本仓库，根目录 `/`。

| 构建配置 | 值 |
|---|---|
| Build command | `bun run build` |
| Deploy command | `bun run deploy` |
| Build variable | `BUN_VERSION=1.4.4` |

`bun run deploy` 会依次完成：

1. **创建 D1 数据库**（已存在则跳过）
2. **执行 `migrations/` 下未应用的 SQL**（`0001` 建库，`0002` 包含本次全部数据库改动，已执行过的不会重跑）。部署脚本会检查历史版本是否已添加 `push_status`：若存在且类型一致，在临时迁移副本中跳过重复加列，保留原值，其余 SQL 仍由 Wrangler 正常执行并记录；不会改写仓库 SQL。
3. 上传 Worker

所以 **D1 与表结构都是自动建好的**，不需要去控制台 Create，也不用粘 SQL。
R2 的四项配置（见第 6 步）在网页里填。

### 3. 存储划分

| 内容 | 位置 |
|---|---|
| 发件人、收件人、主题、纯文本正文、HTML 正文、附件元数据 | **D1** `emails` 表 |
| 附件字节 | **R2** `mail/{id}/{index}` |
| 完整 MIME 原文 | **R2** `mail/{id}/original.eml` |

D1 的正文按设置里的「正文入库上限」截断；配置完整的 R2 后，新邮件的完整原文另存为 EML，包含超过独立附件大小上限的部件。升级前的历史原文无法补回。

首次收信会先登记恢复任务，再保存原文。附件或入库失败后，Cron 每分钟检查任务并退避重试；附件未恢复时详情页会提示。没有配置 R2 时只保存正文与元数据，无法保留附件或提供原文恢复。

暂时性故障会向运行时报告错误，不调用永久拒收。若 D1 或原文保存步骤失败，不能保证上游 SMTP 自动重投，请检查 Worker 日志；本项目不会把运行时异常当作已经验证的 SMTP 重试承诺。

### 4. 设三个 Secret

**Settings → Variables and Secrets**，全部选 **Secret**：

| 名称 | 说明 |
|---|---|
| `ADMIN_USERNAME` | 管理员用户名，登录时使用 |
| `ADMIN_PASSWORD` | 管理员密码，建议 16 位以上 |
| `JWT_SECRET` | `openssl rand -base64 48` 生成，**生成一次后不要再改**（改了所有登录失效） |

**只有这三项在环境变量里。** 其余配置（Turnstile、推送、收信限额、分页、
会话时长）全部存在 D1，部署后在网页「设置」里改，即时生效，不用重新部署。

### 5. 收信

**Email Routing → Custom addresses → Create address**：

- Custom address 填前缀（如 `mail`）
- Action 选 **Send to a Worker**，Worker 选 `mailflare`

首次启用需按提示加 MX / TXT 记录。发一封测试邮件，几秒后刷新后台即可看到。

### 6. 收件地址策略

**MailFlare 自身不限制收件地址**：只要邮件被路由到本 Worker，发往本域
**任意地址**都会收下并入库，不需要在这里事先注册或创建邮箱。
（`anyone@你的域名`、`noreply@你的域名` 都会进同一个收件箱。）

邮件按 **Message-ID + 信封收件地址** 去重：同一封信投给两个别名时分别保存，
投给同一地址的重复邮件只保留一份。收件地址以实际投递的信封地址为准，
不依赖邮件正文里的 To / Cc 头；没有 Message-ID 时按原文字节和信封收件地址识别重复。

> Email Routing 侧仍需把地址指过来。逐个添加很麻烦时，用
> **Email Routing → Routes → Catch-all address** 设为 *Send to a Worker*，
> 之后本域所有地址都会转给 Worker，由 MailFlare 决定收还是丢。

如果只想收特定地址，登录后在 **设置 → 收信过滤** 里填白名单；
想挡掉骚扰来源就填黑名单。两者都支持三种写法：

| 写法 | 含义 |
|---|---|
| `a@b.com` | 精确匹配该地址 |
| `@b.com` | 匹配 `b.com` 及其所有子域 |
| `b.com` | 等价于 `@b.com` |

命中黑名单或不在白名单内时**静默丢弃**，不回退信——退信等于告诉对方
这个地址真实存在，反而会招来更多垃圾邮件。

### 7. 按需在后台配置

登录后点左下角 **设置**：

| 分组 | 说明 |
|---|---|
| 外观 | 站点名称、Logo、登录页背景（均为 https 图片地址） |
| Turnstile | Site Key + Secret 都填才会在登录页显示验证框；留空则只剩密码一道防线 |
| Telegram / Bark | 验证码推送渠道，Token 留空表示不修改 |
| R2 存储 | 账号 ID、Access Key、Secret Key、桶名（附件都放在桶内 `mail/`） |
| 收信过滤 | 发件人黑名单、收件人白名单 |
| 收信 | 单封大小上限、正文截断阈值、附件上限、上传并发 |
| 邮件列表 | 每页条数 |

Bark 在「设置 → 消息通知」里启用：服务地址默认 `https://api.day.app`，
也可以填写自建服务的根地址；Device Key 单独填写，留空保存不会覆盖已存的 Key。
保存后点击「测试」检查通知。验证码推送和收信失败告警都会使用已启用的渠道。

验证码提取由详情页与通知共用：依次检查主题、纯文本和 HTML，支持数字、大写字母数字混合及连字符分组码。4 位数字和空格分组码只在明确的验证语境下识别；空格分组会合并成连续数字，保留前导零。邮编、订单号、优惠码等语境会被排除；同等可信但不同的候选不自动选取。每部分最多分析前 4000 个文本字符，复杂排版或缺少验证关键词的邮件仍可能漏提，可查看原文。

验证码按渠道独立处理：成功渠道不重发，失败渠道最多尝试 3 轮，间隔 1、2 分钟；从实际收信起超过 10 分钟就停止。渠道禁用或邮件已进回收站时跳过。网络请求超时或进程在发送成功后中断，可能出现重复通知，无法保证恰好一次送达。渠道原有的 POST/GET 或格式兼容回退可能在一轮中发起多个请求。

## 回收站、导出与备份

邮件详情中的「删除」移入回收站，可随时恢复；「彻底删除」需要再次确认，清理原文、附件和数据库记录。没有自动清空期限。尚在恢复的邮件暂时不能彻底删除；清理中途失败时保留记录，可再次点击「彻底删除」继续。

详情中的「导出原文」下载原始 `.eml`。全量备份包含收件箱与回收站、未改写的正文/元数据、原文和已保存附件，不读取设置或推送密钥：

```bash
export MAILFLARE_URL='https://你的邮箱域名'
# 在本机环境中设置 MAILFLARE_TOKEN 为已登录会话的 token，不要提交或分享它。
bun run backup ./backups/2026-11-06
```

Token 可在登录后的浏览器开发者工具 Application → Local Storage → `mf_token` 读取。备份目录必须不存在；失败的备份没有 `manifest.json` 完成标记。历史缺失原文或未保存附件会列在该文件的 `missing` 中。原始邮件和附件可交给其他邮件客户端或脚本恢复，当前没有一键导回 MailFlare 的界面。备份期间尽量暂停删除、恢复等操作，它是分页导出而非数据库事务快照。

需要包含全部后台设置的灾难恢复备份时，另行使用 Cloudflare D1 导出并备份 R2 桶；D1 导出含密钥，应保存在私人加密存储。不要给 `mail/` 设置会自动过期删除有效邮件原文的桶生命周期规则。

---

## 云端构建说明

`bun run build` 执行 [`scripts/build.sh`](./scripts/build.sh)。Cloudflare 的
构建镜像**没有预装 Rust，也没有 pkg-config**，所以脚本自己负责：

1. 检查产物新鲜度，已构建且源文件未变则跳过（一次部署会调用两次）
2. 用 rustup 装 Rust（minimal profile）
3. 添加 `wasm32-unknown-unknown`
4. 设 `OPENSSL_INCLUDE_DIR` / `OPENSSL_LIB_DIR`，绕开 `openssl-sys`
   对 pkg-config 的探测（缺失会 panic）
5. 装 `worker-build 0.8.7`（在临时目录执行，避开 `rust-toolchain.toml`
   导致的 toolchain 覆盖报错）
6. `bun install --frozen-lockfile` + 构建前端
7. `worker-build --release`

首次约 5–7 分钟，之后命中缓存约 2 分钟。

---

## 本地开发

需要 Bun 1.4.4+ 与 Rust（`rust-toolchain.toml` 会自动装 target）。浏览器测试默认使用 `/usr/bin/chromium`，可通过 `CHROMIUM_PATH` 指定已有 Chrome/Chromium。集成测试使用 Node.js 22+，必须先完成 `bun run build`；所有 R2、Bark、Telegram 请求都由本地模拟服务接管。

```bash
bun install --frozen-lockfile
cp .dev.vars.example .dev.vars   # 填 ADMIN_USERNAME / ADMIN_PASSWORD / JWT_SECRET
bunx wrangler d1 migrations apply DB --local
bun run dev                      # 监听前端 TS，改动自动重建
bunx wrangler dev                # 另开一个终端起本地 Worker
```

| 命令 | 用途 |
|---|---|
| `bun run build` | 完整构建（前端 + wasm） |
| `bun run build:frontend` | 只构建前端 |
| `bun run dev` | 前端监听重建 |
| `bun run deploy` | 构建后部署 |
| `bun run test` | Rust + TS + 列表竞态 + 真实浏览器安全与界面测试 |
| `bun run check:integration` | 构建后用 Miniflare 验证迁移、回收站、存储故障与通知重试 |
| `bun run backup <新目录>` | 导出收件箱、回收站、原文及附件 |
| `bun run lint` | clippy `-D warnings` |
| `bun run clean` | 清理产物 |

---

## 常见问题

| 现象 | 原因 / 解决 |
|---|---|
| 部署报 `cargo: command not found` | 构建命令没用 `bun run build`（它才会装 Rust） |
| 构建报 `pkg-config` / `openssl-sys` | 脚本已设 `OPENSSL_*_DIR`；仍失败说明镜像缺 libssl-dev |
| 构建报 `externref table required` | 别把 `strip` 改回 `true`，会剥掉 `target_features` 段 |
| 部署报找不到 D1 | `database_id` 还是全 0，或没绑 `DB` |
| 登录页报 500 | 三个 Secret 没设全，错误信息会指明缺哪个 |
| 收信没反应 | 检查 Email Routing 路由规则、MX 记录，以及 Worker 的 `Logs` 实时流 |
| 附件报「R2 未配置完整」 | 进「设置 → R2 存储」，四项都要填 |
| 附件上传 403 | Access Key 没有该桶的写权限，或账号 ID 填错 |

---

## 目录结构

```
MailFlare/
├── src/                       # 后端（Rust 2024 + workers-rs）
│   ├── lib.rs                 # fetch / email / scheduled 事件入口
│   ├── api.rs                 # HTTP 路由与 handler
│   ├── auth.rs                # 凭证校验、Turnstile、JWT
│   ├── config.rs              # 环境变量（3 项）+ D1 设置
│   ├── db.rs                  # D1 数据访问
│   ├── inbound.rs             # 收信流水线
│   ├── jobs.rs                # 原文恢复与通知重试任务
│   ├── mail.rs                # mail-parser 解析
│   ├── notify.rs              # Telegram / Bark 推送
│   ├── r2.rs                  # R2 对象读写
│   ├── jwt.rs                 # 手写 HS256（不引入 jsonwebtoken）
│   ├── util.rs                # 时间 / ID / 截断 / 常量时间比较
│   └── tests.rs               # 单元测试
├── web/
│   ├── index.template.html    # HTML + CSS 模板
│   └── src/                   # TypeScript 前端（11 个模块）
├── scripts/
│   ├── build.sh               # 云端构建入口
│   ├── build.ts               # TS → 内联进 public/index.html
│   └── check-render.ts        # 渲染安全自检
├── migrations/                # D1 建表 SQL（部署时自动执行）
├── wrangler.toml              # D1 / R2 / [assets] 绑定
└── public/index.html          # 构建产物（已 gitignore）
```

---

## 设计取舍

- **密码走 SHA-256 + 常量时间比较**，且用户名错误时不提前返回，避免用响应时间探测用户名
- **附件双凭证**：`scp=api`（Bearer）+ `scp=dl`（10 分钟下载票据），因为 `<img src>` 无法带 Authorization 头
- **JWT 手写**，只要 HS256，不拖入 `rsa`/`p256` 与 `getrandom` 传递依赖；`alg` 写死，免疫 `alg=none` 攻击
- **邮件 HTML 在 `<iframe sandbox>` 里渲染**，拦截外部追踪图片，外链强制 `target="_blank"` + `rel="noopener noreferrer"`
- **D1 行长兜底**：正文超限按 UTF-8 边界截断，避免单行超限导致整封邮件入库失败
- **收信失败告警**：尝试通过已启用渠道提示异常；存储或通知同时不可用时依赖 Worker 日志。
- **登录限流**：Cloudflare 注入的来源 IP 哈希后计数，5 分钟最多 5 次尝试；一个来源不会锁住所有管理员登录。
