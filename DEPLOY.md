# 部署指南（Cloudflare 网页控制台 + GitHub）

全部在 Cloudflare 网页上完成，不需要本地装 Rust 或 wrangler。
适合「连 GitHub → 填参数 → 点部署」的流程。

> 本项目参考了同目录下 `bark-worker` 与 `ImgFlare` 的云端构建实践。

---

## 0. 前置条件

- GitHub 仓库已推送
- Cloudflare 账号已添加你的域名（Email Routing 需要）

**不要**把任何密钥写进仓库——全部通过控制台的 Secret 设置。

---

## 1. 创建 D1 数据库

控制台 → **Workers & Pages** → **D1 SQL Database** → **Create**

| 项 | 值 |
|---|---|
| Name | `mailflare` |
| Location | 就近选 |

创建后复制 **Database ID**（形如 `a1b2c3d4-...`）。

### 1.1 初始化表结构

进入数据库 → **Console** 标签页，粘贴 `schema.sql` 全部内容后执行。

验证：

```sql
SELECT name FROM sqlite_master WHERE type='table';
-- 应看到：configs, emails
```

---

## 2. 创建 R2 存储桶

控制台 → **R2 Object Storage** → **Create bucket**

| 项 | 值 |
|---|---|
| Bucket name | `mailflare-mail` |

> R2 需要先在账号里启用（要绑支付方式，有免费额度）。

---

## 3. 创建 Worker 并连接 GitHub

控制台 → **Workers & Pages** → **Create** → **Workers** → **Connect to Git**

1. 选择本仓库，分支 `main`，Root directory `/`
2. 构建配置：

| 项 | 值 |
|---|---|
| Build command | `bun run build` |
| Deploy command | `bunx wrangler deploy` |
| Build variable | `BUN_VERSION=1.4.3` |

> **Build variable 放在 Settings → Build → Build variables and secrets**，
> 只在构建期可见，不会进入运行时。Bun 版本固定可避免上游更新导致行为漂移。

### 3.1 关于云端构建 Rust

Cloudflare 的构建镜像**没有预装 Rust 工具链，也没有 `pkg-config`**。

`bun run build` 会执行 `scripts/build.sh`，由它负责：

| 步骤 | 说明 |
|---|---|
| 检查产物新鲜度 | 已构建且源文件未变时直接跳过（一次部署可能调用两次） |
| 安装 Rust | 通过 rustup 官方脚本，`--profile minimal`（首次约 1 分钟） |
| 添加 wasm32 target | `rustup target add wasm32-unknown-unknown` |
| 绕开 pkg-config | 显式设置 `OPENSSL_INCLUDE_DIR` / `OPENSSL_LIB_DIR` |
| 安装 worker-build | `cargo install worker-build --version 0.8.7 --locked`（首次 3–5 分钟） |
| 构建前端 | `bun install --frozen-lockfile && bun run build:frontend` |
| 编译 wasm | `worker-build --release` |

首次部署约 5–7 分钟，之后命中构建缓存约 2 分钟。

> **为什么必须先装 Rust**：镜像里 `cargo`/`rustup` 都不存在，
> 构建命令直接写 `cargo install ...` 会以 `command not found` 失败。

> **为什么要设 OpenSSL 路径**：`worker-build` 的依赖链里有 `openssl-sys`，
> 它默认用 `pkg-config` 探测。镜像没有 pkg-config，探测会 panic。

---

## 4. 绑定 D1 与 R2

Worker → **Settings** → **Bindings**

### 4.1 D1

**Add** → **D1 database**

| 项 | 值 |
|---|---|
| Variable name | `DB` |
| D1 database | 选第 1 步创建的 `mailflare` |

### 4.2 R2

**Add** → **R2 bucket**

| 项 | 值 |
|---|---|
| Variable name | `R2` |
| R2 bucket | 选 `mailflare-mail` |

> 名字**必须**是 `DB` 和 `R2` —— 代码按这两个名字取绑定。

---

## 5. 设置密钥与变量

Worker → **Settings** → **Variables and Secrets**

### 5.1 必填（类型选 **Secret**）

| Name | 值 |
|---|---|
| `ADMIN_PASSWORD` | 管理员密码（16 位以上） |
| `JWT_SECRET` | `openssl rand -base64 48` 或 `openssl rand -hex 32` |

> `JWT_SECRET` 生成一次后**保持不变**。修改它会让所有已登录会话失效。

### 5.2 推荐（类型选 **Secret**）

| Name | 值 |
|---|---|
| `TURNSTILE_SECRET` | Turnstile Secret Key（见第 6 步） |

> 留空则**跳过人机验证**。生产环境务必设置，否则登录只有密码一道防线。

### 5.3 普通变量（**Plaintext**）

`wrangler.toml` 的 `[vars]` 已有默认值，控制台设置会覆盖。

| Name | 示例 |
|---|---|
| `TURNSTILE_SITE_KEY` | `0x4AAA...` |
| `PUBLIC_URL` | `https://mail.example.com` |

其余（`MAX_BODY_BYTES`、`R2_CONCURRENCY` 等）保持默认即可。

---

## 6. 配置 Turnstile（推荐）

控制台 → **Turnstile** → **Add widget**

| 项 | 值 |
|---|---|
| Hostnames | Worker 域名 + 自定义域名 |
| Widget Mode | **Managed** |

- **Site Key** → 填第 5.3 步的 `TURNSTILE_SITE_KEY`
- **Secret Key** → 填第 5.2 步的 `TURNSTILE_SECRET`

---

## 7. 部署

点 **Deploy**（或推送 commit 触发）。成功后访问 Worker 域名，应看到登录页。

### 7.1 自检

| 检查项 | 方法 | 预期 |
|---|---|---|
| API 活着 | `/api/health` | `{"ok":true,"data":{"service":"mailflare",...}}` |
| 前端加载 | `/` | 登录页，无 JS 报错 |
| Turnstile | 登录页 | 出现验证框（若配了 secret） |
| 登录 | 输入密码 | 进入邮件列表 |

`/api/health` 返回 500 多半是 `ADMIN_PASSWORD` 或 `JWT_SECRET` 没设，
错误信息会指明缺哪个。

---

## 8. 配置收信（Email Routing）

控制台 → 你的域名 → **Email** → **Email Routing**

### 8.1 启用

首次需点 **Enable**，按提示添加 MX 与 TXT 记录（Cloudflare 自动加）。

### 8.2 添加路由

**Routing rules** → **Custom addresses** → **Create address**

| 项 | 值 |
|---|---|
| Custom address | 例如 `mail`（即 `mail@你的域名`） |
| Action | **Send to a Worker** |
| Worker | `mailflare` |

发一封测试邮件，几秒后刷新后台应能看到。

### 8.3 收不到邮件？

按顺序排查：

1. Worker → **Logs** → **Begin log stream**，发信时看有没有 `[mailflare]` 输出
2. 日志显示「配置缺失」→ 检查第 5 步 Secret
3. 日志显示「邮件入库失败」→ 检查 D1 绑定名是否为 `DB`、`schema.sql` 是否执行
4. 收到 TG/Bark 告警 → 按告警里的失败类型处理
5. 完全没有日志 → Email Routing 的 MX 记录未生效（等 5–10 分钟）

> 注意：本项目的 `[observability]` 已关闭（不需要 Dashboard 日志留存）。
> 排障时用 `Logs` 页面的实时流即可，或本地 `bunx wrangler tail`。

---

## 9. 推送通知

部署后**在网页里配置**，不需要改代码：

登录后台 → 左下角 **通知设置**

### Telegram

1. 找 [@BotFather](https://t.me/BotFather) 建 bot，拿 token（`123456789:ABCdef...`）
2. 给 bot 发条消息，访问 `https://api.telegram.org/bot<token>/getUpdates` 拿 `chat_id`
3. 填入设置页，勾选「启用 Telegram 推送」

### Bark

1. Bark App 里复制设备 Key
2. 服务地址默认 `https://api.day.app`（自建可改）
3. 填入设置页，勾选「启用 Bark 推送」

> Token 保存后只显示尾 4 位；输入框留空表示「不修改」。

---

## 10. 更新部署

推代码即自动重新部署：

```bash
git add -A && git commit -m "..." && git push
```

回滚用 **Deployments** 页面的 **Rollback**。

---

## 附：常见部署错误

| 错误 | 原因 | 解决 |
|---|---|---|
| `cargo: command not found` | 构建命令没先装 Rust | 用 `bun run build`（走 `scripts/build.sh`），不要直接写 `cargo install` |
| `pkg-config` / `openssl-sys` panic | 镜像无 pkg-config | 脚本已设 `OPENSSL_*_DIR`；若仍失败，检查镜像是否有 libssl-dev |
| `Couldn't find a D1 DB ... 'DB'` | 没绑 D1 或名字不对 | 第 4.1 步，变量名必须是 `DB` |
| `database_id` 全 0 | 没替换占位符 | 用第 1 步的真实 ID |
| `externref table required for catch wrappers` | `strip = true` 剥离了 `target_features` 段 | 本项目已用 `strip = "debuginfo"`，不要改回 `true` |
| 构建超时 | 首次装 Rust + worker-build 较慢 | 重新触发一次，缓存会生效 |
| 登录 500 且日志说缺变量 | Secret 没设 | 第 5 步 |
| 收信没反应 | Email Routing 没配或 MX 未生效 | 第 8 步 |
| 附件 404 | R2 绑定名不是 `R2` | 第 4.2 步 |

---

## 本地开发（可选）

需要 Bun 1.4.3+ 与 Rust（含 `wasm32-unknown-unknown`，由 `rust-toolchain.toml` 自动装）。

```bash
bun install --frozen-lockfile
cp .dev.vars.example .dev.vars    # 填写本地配置
bun run dev                        # 监听前端 TS，改动自动重建
bunx wrangler dev                  # 启动本地 Worker
bunx wrangler d1 execute mailflare --local --file=./schema.sql
```

| 命令 | 用途 |
|---|---|
| `bun run build` | 完整构建（前端 + wasm） |
| `bun run build:frontend` | 只构建前端 |
| `bun run deploy` | 构建后部署到 Cloudflare |
| `bun run test` | Rust 测试 + TS 类型检查 + 渲染自检 |
| `bun run lint` | clippy（-D warnings） |
| `bun run clean` | 清理构建产物 |
