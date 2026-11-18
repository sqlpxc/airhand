#!/usr/bin/env bash
# Linux 上跑全部测试：单元测试（含网页控制台） + X11 采集/输入 + 声音环回 + 端到端 + 前端。
#
# X11 相关的测试会开全屏窗口、注入键鼠，所以一律放在单独的 Xvfb 虚拟屏幕上跑，不影响当前桌面。
# 声音测试需要 PulseAudio 或 PipeWire（WSLg 自带），没有时跳过。
#
#   bash client/scripts/linux-test.sh
#
# 需要：Xvfb（xvfb 包）、Node.js 20+，以及 README"构建客户端"一节里的 Linux 依赖。
# 在 WSL 里使用时，建议先把仓库复制到 WSL 自己的磁盘上（例如 ~/airhand）再运行，/mnt 下编译很慢。
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CLIENT=$ROOT/client/src-tauri
PIDS=()
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
}
trap cleanup EXIT
step() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }

# 找一个空闲的显示编号，启动虚拟屏幕
n=99
while [ -e /tmp/.X11-unix/X$n ] || [ -e /tmp/.X$n-lock ]; do n=$((n + 1)); done
Xvfb :$n -screen 0 1920x1080x24 -nolisten tcp >/dev/null 2>&1 &
PIDS+=($!)
export DISPLAY=:$n
# 原生 Wayland 的 GTK 程序不会出现在 Xvfb 上
export WAYLAND_DISPLAY=
for _ in $(seq 50); do [ -e /tmp/.X11-unix/X$n ] && break; sleep 0.1; done
# 让屏幕上有东西在动（端到端测试要收到画面变化）
if command -v xclock >/dev/null; then
  xclock -update 1 -geometry 300x300+100+100 >/dev/null 2>&1 &
  PIDS+=($!)
fi

step "协议 + 链路库 + 服务器"
cd "$ROOT"
cargo test --quiet
cargo build --quiet -p airhand-server

step "网页控制台"
cd "$ROOT/web"
cargo test --quiet

step "客户端单元测试"
cd "$CLIENT"
cargo test --quiet --lib

step "X11 采集与输入注入（虚拟屏幕 $DISPLAY）"
cargo test --quiet --lib x11_ -- --ignored --test-threads=1

if command -v pactl >/dev/null && pactl info >/dev/null 2>&1; then
  step "声音环回采集（-60 dBFS 测试音）"
  cargo test --quiet --lib loopback_captures_tone -- --ignored
else
  step "跳过声音测试：没有 PulseAudio/PipeWire"
fi

step "端到端：真实服务器 + 被控端 + 协议级控制端"
mkdir -p "$ROOT/target/e2e"
AIRHAND_LISTEN=127.0.0.1:4433 AIRHAND_CERT="$ROOT/target/e2e/cert.pem" AIRHAND_KEY="$ROOT/target/e2e/key.pem" \
  RUST_LOG=warn "$ROOT/target/debug/airhand-server" &
SERVER=$!
PIDS+=($SERVER)
sleep 1
cargo test --quiet --lib e2e_roundtrip -- --ignored
if command -v parec >/dev/null && command -v pw-cli >/dev/null && pw-cli info 0 >/dev/null 2>&1 && pactl info >/dev/null 2>&1; then
  step "麦克风转发、断线与卸载清理（PipeWire）"
  cargo test --quiet --lib e2e_mic -- --ignored
  cargo test --quiet --lib native_mic_cleanup -- --ignored
fi
kill $SERVER
wait $SERVER 2>/dev/null || true
# 这个测试自己启动/重启服务器
cargo test --quiet --lib e2e_reconnect -- --ignored

step "前端"
cd "$ROOT/client"
[ -d node_modules ] || npm ci --no-audit --no-fund
npm run --silent typecheck
npm test

step "全部通过"
