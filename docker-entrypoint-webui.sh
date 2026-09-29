#!/usr/bin/env bash
set -euo pipefail

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

exec /app/unified-novel-downloader "${args[@]}" "$@"
