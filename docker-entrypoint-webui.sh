#!/usr/bin/env bash
set -uo pipefail

args=()

has_server=false
for arg in "$@"; do
  if [ "$arg" = "--server" ]; then
    has_server=true
    break
  fi
done

if [ "$has_server" = false ]; then
  args+=("--server")
fi

# 数据目录与密码锁：优先读 UNIFIED_*，并兼容改名前的 TOMATO_* 变量。
data_dir="${UNIFIED_DATA_DIR:-${TOMATO_DATA_DIR:-}}"
if [ -n "$data_dir" ]; then
  args+=("--data-dir" "$data_dir")
fi

web_password="${UNIFIED_WEB_PASSWORD:-${TOMATO_WEB_PASSWORD:-}}"
if [ -n "$web_password" ]; then
  args+=("--password" "$web_password")
fi

# ── 番茄签名 sidecar：容器内后台拉起 unidbg 服务，供 Rust 直连自签 ──
if [ "${SIDECAR_ENABLED:-1}" = "1" ] && [ -f /app/sidecar.jar ]; then
  sidecar_port="${SIDECAR_PORT:-8099}"
  # shellcheck disable=SC2086
  java ${SIDECAR_JVM_OPTS:-"-Xms64m -Xmx512m"} \
    -jar /app/sidecar.jar --server.port="$sidecar_port" &
  SIDECAR_PID=$!

  # 等待就绪（最多 90s）；/dev/tcp 探测端口，避免依赖 curl/wget。
  ready=false
  for _ in $(seq 1 90); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$sidecar_port") 2>/dev/null; then
      exec 3>&- 3<&- || true
      ready=true
      break
    fi
    sleep 1
  done
  if [ "$ready" = true ]; then
    echo "[sidecar] ready on :$sidecar_port (pid=$SIDECAR_PID)"
  else
    echo "[sidecar] WARN 未在 90s 内就绪；番茄自签可能不可用（其余源不受影响）"
  fi
fi

exec /app/unified-novel-downloader "${args[@]}" "$@"
