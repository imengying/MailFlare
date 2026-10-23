# 部署指南（Cloudflare 网页控制台 + GitHub）

全部在 Cloudflare 网页上完成，不需要本地装 wrangler。
适合「连 GitHub → 填参数 → 点部署」的流程。

---

## 0. 前置条件

- GitHub 仓库已推送（本项目已推到 `imengying/MailFlare`）
- Cloudflare 账号已添加你的域名（Email Routing 需要）
- **不要**把任何密钥写进仓库——下面全部通过控制台的 Secret 设置

---

## 1. 创建 D1 数据库

控制台 → **Workers & Pages** → 左侧 **D1 SQL Database** → **Create**

| 项 | 值 |
|---|---|
| Name | `mailflare` |
| Location | 就近选（如 APAC） |

创建后复制 **Database ID**（形如 `a1b2c3d4-...`），下一步要用。

### 1.1 初始化表结构

进入刚创建的数据库 → **Console** 标签页，粘贴 `schema.sql` 全部内容后执行。

或者用 **Execute** 面板逐条跑。执行完在 Console 里验证：

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
| Location | 就近选 |

> R2 需要先在账号里**启用**（要绑支付方式，但有免费额度）。
> 附件与原始 `.eml` 都存这里。

---

## 3. 创建 Worker 并连接 GitHub

控制台 → **Workers & Pages** → **Create** → **Workers** → **Connect to Git**

1. 授权 GitHub，选择 `imengying/MailFlare` 仓库
2. 分支选 `main`
3. 构建设置：

| 项 | 值 |
|---|---|
| Build command | 留空（由 `wrangler.toml` 的 `[build].command` 接管） |
| Deploy command | `npx wrangler deploy` |
| Root directory | `/` |

> **为什么不填 Build command**：`wrangler deploy` 会读取 `wrangler.toml` 里的
> `[build].command` 并在云端执行它。这样本地和云端用同一套构建逻辑，
> 不会出现「本地能构建、云端不行」。

### 3.1 关于云端构建 Rust

Cloudflare 的构建镜像**内置 Rust 与 wasm32 target**，但没有 `worker-build`。
`wrangler.toml` 里的构建命令已经包含：

```
rustup target add wasm32-unknown-unknown &&
cargo install worker-build --version 0.8.7 --locked &&
bun install &&
bun run build &&
worker-build --release
```

首次构建大约 3–5 分钟（`cargo install` 要编译 worker-build），
之后会命中构建缓存，约 1–2 分钟。

> 如果云端构建报「找不到 cargo / rustup」：在 Worker 的
> **Settings → Build → Build variables** 里确认没有覆盖 `PATH`，
> 并检查构建日志开头的工具链版本。Cloudflare 偶尔会调整镜像，
> 届时可在 Build command 里显式加 `rustup default stable`。

---

## 4. 绑定 D1 与 R2

Worker → **Settings** → **Bindings**（或 **Functions → Bindings**）

### 4.1 D1 绑定

**Add** → **D1 database**

| 项 | 值 |
|---|---|
| Variable name | `DB` |
| D1 database | 选第 1 步创建的 `mailflare` |

### 4.2 R2 绑定

**Add** → **R2 bucket**

| 项 | 值 |
|---|---|
| Variable name | `R2` |
| R2 bucket | 选第 2 步创建的 `mailflare-mail` |

> 名字必须是 `DB` 和 `R2` —— 代码里按这两个名字取绑定。

---

## 5. 设置密钥与变量

Worker → **Settings** → **Variables and Secrets**

### 5.1 必填（选 **Secret** 类型，加密存储）

点 **Add** → 类型选 **Secret**：

| Name | 值 |
|---|---|
| `ADMIN_PASSWORD` | 你的管理员密码（建议 16 位以上） |
| `JWT_SECRET` | 随机串，生成方法见下 |

生成 `JWT_SECRET`（任选其一）：

```bash
openssl rand -base64 48
```
或用浏览器控制台：
```js
crypto.getRandomValues(new Uint8Array(48)).join('')
```

### 5.2 推荐（Secret）

| Name | 值 |
|---|---|
| `TURNSTILE_SECRET` | Turnstile 的 Secret Key，见第 6 步 |

> 留空则跳过人机验证。**生产环境务必设置**，否则登录接口只有密码一道防线。

### 5.3 普通变量（**Plaintext** 类型）

这些在 `wrangler.toml` 的 `[vars]` 里已有默认值，控制台设置会覆盖。

| Name | 示例 | 说明 |
|---|---|---|
| `TURNSTILE_SITE_KEY` | `0x4AAA...` | Turnstile 公开 site key，前端用来渲染挂件 |
| `PUBLIC_URL` | `https://mail.example.com` | 推送消息里「查看详情」链接的地址 |

其余变量（`MAX_BODY_BYTES`、`R2_CONCURRENCY` 等）保持默认即可。

---

## 6. 配置 Turnstile（推荐）

控制台 → **Turnstile** → **Add widget**

| 项 | 值 |
|---|---|
| Widget name | `MailFlare` |
| Hostnames | 你的 Worker 域名，如 `mailflare.xxx.workers.dev` 和自定义域名 |
| Widget Mode | **Managed** |

创建后会给出两个值：

- **Site Key** → 填到第 5.3 步的 `TURNSTILE_SITE_KEY`
- **Secret Key** → 填到第 5.2 步的 `TURNSTILE_SECRET`

---

## 7. 部署

回到 Worker 的 **Deployments** 页面，点 **Deploy**（或推送一次 commit 触发）。

首次部署成功后，访问 Worker 域名，应该看到登录页。

### 7.1 自检

| 检查项 | 方法 | 预期 |
|---|---|---|
| API 活着 | 访问 `/api/health` | `{"ok":true,"data":{"service":"mailflare",...}}` |
| 前端加载 | 访问 `/` | 出现登录页，无 JS 报错 |
| Turnstile | 登录页 | 出现人机验证框（若配了 secret） |
| 登录 | 输入密码 | 进入邮件列表 |

如果 `/api/health` 返回 500，多半是 `ADMIN_PASSWORD` 或 `JWT_SECRET` 没设。
错误信息里会明确指出缺哪个变量。

---

## 8. 配置收信（Email Routing）

控制台 → 你的域名 → **Email** → **Email Routing**

### 8.1 启用 Email Routing

首次使用需点 **Enable**，按提示添加 MX 与 TXT 记录（Cloudflare 会自动加）。

### 8.2 添加路由规则

**Routing rules** → **Custom addresses** → **Create address**

| 项 | 值 |
|---|---|
| Custom address | 例如 `mail`（即 `mail@你的域名`） |
| Action | **Send to a Worker** |
| Worker | 选 `mailflare` |

保存后，发一封测试邮件到 `mail@你的域名`，几秒后刷新后台应该能看到。

### 8.3 收不到邮件？

按顺序排查：

1. **Worker 日志**（控制台 → Worker → **Logs** → **Begin log stream**）
   发信时观察有没有 `[mailflare]` 开头的输出
2. 若日志显示「配置缺失」→ 检查第 5 步的 Secret 是否都在
3. 若日志显示「邮件入库失败」→ 检查 D1 绑定名是否为 `DB`，
   以及 `schema.sql` 是否执行过
4. 若推送收到告警 → 按告警里的失败类型对症处理
5. 若完全没有日志 → Email Routing 的 MX 记录可能没生效（等 5–10 分钟）

---

## 9. 推送通知配置

部署完成后**在网页里配置**，不需要改代码：

登录后台 → 左下角 **通知设置**

### Telegram

1. 找 [@BotFather](https://t.me/BotFather) 创建 bot，拿到 token
   （形如 `123456789:ABCdef...`）
2. 给 bot 发一条消息，然后访问
   `https://api.telegram.org/bot<token>/getUpdates` 拿到你的 `chat_id`
3. 填入设置页，勾选「启用 Telegram 推送」

### Bark

1. 打开 Bark App，复制设备 Key
2. 服务地址默认 `https://api.day.app`（自建可改）
3. 填入设置页，勾选「启用 Bark 推送」

> Token 保存后只会显示尾 4 位。输入框留空表示「不修改」。

---

## 10. 更新部署

推代码到 GitHub 即自动重新部署：

```bash
git add -A && git commit -m "..." && git push
```

Cloudflare 会在构建日志里显示进度。回滚用 **Deployments** 页面的
**Rollback** 按钮。

---

## 附：常见部署错误

| 错误 | 原因 | 解决 |
|---|---|---|
| `Couldn't find a D1 DB with the name or binding 'DB'` | 没绑 D1 或名字不对 | 第 4.1 步，变量名必须是 `DB` |
| `database_id` 全 0 | 没替换 `wrangler.toml` 里的占位符 | 第 1 步拿到的真实 ID |
| `externref table required for catch wrappers` | `Cargo.toml` 开了 `strip = true` | 移除它（本项目已移除） |
| 构建超时 | `cargo install worker-build` 太慢 | 重新触发一次，缓存会生效 |
| 登录返回 500 且日志说缺变量 | Secret 没设 | 第 5 步 |
| 收信没反应 | Email Routing 没配或 MX 未生效 | 第 8 步 |
| 附件 404 | R2 绑定名不是 `R2` | 第 4.2 步 |
