#!/usr/bin/env bash
# 构建 Android 版 agent —— 不需要 Android NDK。
#
# 原理：编成 aarch64-unknown-linux-musl 的**静态**可执行文件，
# rustc 自带的 rust-lld 就能完成链接，Android 内核可以直接跑这种 ELF。
# 实测真我 Neo8 (arm64-v8a, API 36) 可执行。
#
# 用法：scripts/build-agent-android.sh [aarch64|armv7]
set -euo pipefail

cd "$(dirname "$0")/.."

export CARGO_HOME="${CARGO_HOME:-D:\\PortableApps\\rust\\cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-D:\\PortableApps\\rust\\rustup}"
export PATH="/d/PortableApps/rust/cargo/bin:${PATH}"
# 国内网络直连 static.rust-lang.org 会超时，走字节镜像
export RUSTUP_DIST_SERVER="${RUSTUP_DIST_SERVER:-https://rsproxy.cn}"
export RUSTUP_UPDATE_ROOT="${RUSTUP_UPDATE_ROOT:-https://rsproxy.cn/rustup}"

ARCH="${1:-aarch64}"
case "$ARCH" in
  aarch64) TARGET=aarch64-unknown-linux-musl;   OUT=wsync-agent-aarch64 ;;
  armv7)   TARGET=armv7-unknown-linux-musleabihf; OUT=wsync-agent-armv7 ;;
  *) echo "未知架构: $ARCH（可选 aarch64 / armv7）" >&2; exit 2 ;;
esac

rustup target add "$TARGET" >/dev/null

# rustc 自带的 lld：交叉链接不需要任何外部工具链
HOST=$(rustc -vV | sed -n 's/^host: //p')
LLD="$(rustc --print sysroot)/lib/rustlib/${HOST}/bin/rust-lld"
[ -x "$LLD" ] || [ -f "$LLD.exe" ] || { echo "找不到 rust-lld: $LLD" >&2; exit 1; }

ENV_PREFIX="CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')"
env "${ENV_PREFIX}_LINKER=${LLD}" \
    "${ENV_PREFIX}_RUSTFLAGS=-C link-self-contained=yes" \
    cargo build --release -p wsync-agent --target "$TARGET"

mkdir -p plugins/agent
cp "target/${TARGET}/release/wsync-agent" "plugins/agent/${OUT}"
echo "已生成 plugins/agent/${OUT} ($(wc -c < "plugins/agent/${OUT}") 字节)"
