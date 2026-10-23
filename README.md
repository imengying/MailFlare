# MailFlare ✉️

极简的**单用户**个人邮件系统。一个 Cloudflare Worker 同时收信和当后台：

- **收信**：Email Routing → `mail-parser` 零拷贝解析 → 附件写 R2 → 元数据入库 D1 → `waitUntil()` 异步推送 Telegram / Bark
- **管理**：Rust API + 内嵌单页前端，用户名 + 密码 + Turnstile 登录，无状态 JWT（HS256）
- **存储**：邮件正文与元数据进 D1（`configs` / `emails` 两张表），附件字节进 R2

```
   SMTP ──► Email Routing ──► #[event(email)]
                                ├─► R2   att/{id}/{n}（仅附件字节）
                                ├─► D1   emails
                                └─► waitUntil() ──► Telegram ‖ Bark

   Browser ──► /api/*  ──► #[event(fetch)] ──► D1 / R2
           └─► 其它路径 ──► [assets] 单页前端
```

---

## 部署

全部在 Cloudflare 网页完成，不需要本地装 Rust 或 wrangler。

### 1. 建 R2 桶

**R2 Object Storage → Create bucket**，名字必须是 `mailflare-mail`。

### 2. 连 GitHub 部署

**Workers & Pages → Create → Workers → Connect to Git**，选本仓库，根目录 `/`。

| 构建配置 | 值 |
|---|---|
| Build command | `bun run build` |
| Deploy command | `bun run deploy` |
| Build variable | `BUN_VERSION=1.4.3` |

`bun run deploy` 会依次完成：

1. **创建 D1 数据库**（已存在则跳过）
2. **执行 `migrations/` 下的建表 SQL**（已执行过的不会重跑）
3. 上传 Worker

所以 **D1 与表结构都是自动建好的**，不需要去控制台 Create，也不用粘 SQL。

绑定关系在 `wrangler.toml` 里已声明（`DB` / `R2`），部署时按名字自动匹配。

### 3. 存储划分

| 内容 | 位置 |
|---|---|
| 发件人、收件人、主题、纯文本正文、HTML 正文、附件元数据 | **D1** `emails` 表 |
| 附件原始字节 | **R2** `att/{id}/{index}` |

邮件正文完整保留在 D1（超长时按设置里的「正文入库上限」截断），
R2 只承担附件，这样 D1 的单行长度不会被附件撑爆。

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
| 站点地址 | 推送消息里「查看详情」链接的前缀，留空则不带链接 |
| Turnstile | Site Key + Secret 都填才会在登录页显示验证框；留空则只剩密码一道防线 |
| Telegram / Bark | 推送渠道，Token 留空表示不修改 |
| 收信过滤 | 发件人黑名单、收件人白名单 |
| 收信 | 单封大小上限、正文截断阈值、附件上限、上传并发 |
| 列表与会话 | 每页条数、登录有效期 |

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

需要 Bun 1.4.3+ 与 Rust（`rust-toolchain.toml` 会自动装 target）。

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
| `bun run test` | Rust 测试 + TS 类型检查 + 渲染自检 |
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
| 附件 404 | R2 绑定名不是 `R2` |

---

## 目录结构

```
MailFlare/
├── src/                       # 后端（Rust 2024 + workers-rs）
│   ├── lib.rs                 # fetch / email 事件入口
│   ├── api.rs                 # HTTP 路由与 handler
│   ├── auth.rs                # 凭证校验、Turnstile、JWT
│   ├── config.rs              # 环境变量（3 项）+ D1 设置
│   ├── db.rs                  # D1 数据访问
│   ├── inbound.rs             # 收信流水线
│   ├── mail.rs                # mail-parser 解析
│   ├── notify.rs              # Telegram / Bark 并发推送
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
- **收信失败告警**：入库/存储异常会推一条 TG/Bark 消息，不静默丢信
