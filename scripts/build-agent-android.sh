#!/usr/bin/env bash
# 构建 Android 版接收端 —— 不需要 Android NDK。
#
# 原理：编成 aarch64-unknown-linux-musl 的**静态**可执行文件，
# rustc 自带的 rust-lld 就能完成链接，Android 内核可以直接跑这种 ELF。
# 实测在 arm64-v8a / API 36 的真机上运行正常，体积约 440 KB。
#
# 用法：scripts/build-agent-android.sh [aarch64|armv7]
#
# 前置：已安装 Rust（rustup.rs）。首次运行会自动下载对应 target 的 std。
# 国内网络直连 static.rust-lang.org 容易超时，可先设置镜像：
#     export RUSTUP_DIST_SERVER=https://rsproxy.cn
#     export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
set -euo pipefail

cd "$(dirname "$0")/.."

# cargo 常装在 ~/.cargo/bin 或 $CARGO_HOME/bin，非登录 shell 里可能不在 PATH
if ! command -v cargo >/dev/null 2>&1; then
    for p in "${CARGO_HOME:-$HOME/.cargo}/bin" /usr/local/cargo/bin; do
        [ -x "$p/cargo" ] && PATH="$p:$PATH"
    done
fi
command -v cargo >/dev/null 2>&1 || {
    echo "找不到 cargo，请先安装 Rust：https://rustup.rs" >&2
    exit 1
}

ARCH="${1:-aarch64}"
case "$ARCH" in
  aarch64) TARGET=aarch64-unknown-linux-musl;    OUT=wsync-agent-aarch64 ;;
  armv7)   TARGET=armv7-unknown-linux-musleabihf; OUT=wsync-agent-armv7 ;;
  *) echo "未知架构: $ARCH（可选 aarch64 / armv7）" >&2; exit 2 ;;
esac

rustup target add "$TARGET" >/dev/null

# rustc 自带的 lld：交叉链接不需要任何外部工具链
HOST=$(rustc -vV | sed -n 's/^host: //p')
LLD="$(rustc --print sysroot)/lib/rustlib/${HOST}/bin/rust-lld"
[ -f "${LLD}.exe" ] && LLD="${LLD}.exe"
[ -f "$LLD" ] || { echo "找不到 rust-lld: $LLD" >&2; exit 1; }

ENV_PREFIX="CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')"
export "${ENV_PREFIX}_LINKER=${LLD}"
export "${ENV_PREFIX}_RUSTFLAGS=-C link-self-contained=yes"
cargo build --release -p wsync-agent --target "$TARGET"

mkdir -p plugins/agent
cp "target/${TARGET}/release/wsync-agent" "plugins/agent/${OUT}"
echo "已生成 plugins/agent/${OUT}（$(wc -c < "plugins/agent/${OUT}") 字节）"
