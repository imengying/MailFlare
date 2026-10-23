#!/usr/bin/env bash
# =============================================================================
# MailFlare 云端构建脚本
#
# 供 Cloudflare Workers Builds（连 GitHub 自动部署）调用。
# 本地开发也可以直接跑 `bun run build`。
#
# ## 为什么需要这个脚本
# Cloudflare 的构建镜像是 Ubuntu，**没有预装 Rust 工具链，也没有 pkg-config**。
# 所以构建命令不能直接写 `cargo install ...`，必须先自己把 Rust 装上。
#
# ## 关于 pkg-config 与 OpenSSL
# `worker-build` 依赖链里有 `openssl-sys`（经 ureq/native-tls），它默认用
# `pkg-config` 探测 OpenSSL。镜像是没有 pkg-config 的，探测会 panic，
# 因此这里显式给出头文件与库路径。
#
# ## 关于重复构建
# 一次部署中本脚本可能被调用两次（Dashboard 的 Build command 一次，
# `wrangler deploy` 里的 `[build].command` 一次）。所以开头做产物新鲜度检查，
# 已构建且源文件未变时直接跳过。
# =============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }

# -----------------------------------------------------------------------------
# 1. 幂等：产物比所有源文件新就直接复用
# -----------------------------------------------------------------------------
if [ -f build/worker/shim.mjs ] && [ -f public/index.html ] \
   && [ -z "$(find src Cargo.toml Cargo.lock web scripts public/index.html -newer build/worker/shim.mjs 2>/dev/null)" ]; then
  log "已是最新构建，跳过：build/worker/shim.mjs"
  exit 0
fi

# -----------------------------------------------------------------------------
# 2. 确保 Rust 工具链存在
# -----------------------------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  log "未检测到 Rust，正在安装工具链（首次约 1 分钟）"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
log "Rust: $(cargo --version)"

if ! rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown; then
  log "添加 wasm32-unknown-unknown"
  rustup target add wasm32-unknown-unknown
fi

# -----------------------------------------------------------------------------
# 3. 绕过 pkg-config 探测 OpenSSL
#
# 只在路径真实存在时设置，避免在已有 pkg-config 的环境里帮倒忙。
# 这是 bark-worker 项目验证过的做法。
# -----------------------------------------------------------------------------
if [ -z "${OPENSSL_INCLUDE_DIR:-}" ] && [ -d /usr/include/openssl ]; then
  export OPENSSL_INCLUDE_DIR=/usr/include
fi
if [ -z "${OPENSSL_LIB_DIR:-}" ]; then
  for dir in /usr/lib/x86_64-linux-gnu /usr/lib/aarch64-linux-gnu /usr/lib64; do
    if [ -d "$dir" ] && ls "$dir"/libssl.so* >/dev/null 2>&1; then
      export OPENSSL_LIB_DIR="$dir"
      break
    fi
  done
fi
[ -n "${OPENSSL_LIB_DIR:-}" ] && log "OpenSSL: include=${OPENSSL_INCLUDE_DIR:-auto} lib=$OPENSSL_LIB_DIR"

# -----------------------------------------------------------------------------
# 4. 确保 worker-build 存在
# -----------------------------------------------------------------------------
if ! command -v worker-build >/dev/null 2>&1; then
  log "安装 worker-build 0.8.7（首次约 3-5 分钟，之后走缓存）"
  # 在临时目录里安装：本仓库的 rust-toolchain.toml 会让 rustup 报
  # "default toolchain implicitly overridden"，换目录可彻底避开。
  ( cd "$(mktemp -d)" && cargo +stable install worker-build --version 0.8.7 --locked )
fi

# -----------------------------------------------------------------------------
# 5. 前端：TypeScript -> 内联进 public/index.html
# -----------------------------------------------------------------------------
log "构建前端"
if ! command -v bun >/dev/null 2>&1; then
  log "未检测到 bun，正在安装"
  curl -fsSL https://bun.sh/install | bash
  export PATH="$HOME/.bun/bin:$PATH"
fi
bun install --frozen-lockfile
# 注意：这里必须调 `build:frontend`，不能调 `build`——
# 后者就是本脚本自己，会无限递归。
bun run build:frontend

# -----------------------------------------------------------------------------
# 6. Rust -> wasm
# -----------------------------------------------------------------------------
log "编译 Worker（release）"
worker-build --release

log "完成：build/worker/shim.mjs"
