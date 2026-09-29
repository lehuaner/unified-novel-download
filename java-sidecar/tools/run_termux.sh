#!/usr/bin/env bash
# Termux/Android 一次性运行 sidecar：只“下载预构建 jar + 启动”，不在手机编译，也不需要 APK。
# 签名所需 .so/ms.bin 已随 jar 打包（classpath），运行期无外部资源依赖。
#
# 环境变量（按需）：
#   JAVA_BIN         java 可执行文件路径（默认 command -v java）
#   SIDECAR_JAR_URL  预构建 jar 下载地址（本地缺失时下载到 target/）
#   SIDECAR_PORT     监听端口（默认 8099）
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"          # java-sidecar
JAR="$HERE/target/unidbg-boot-server-0.0.1-SNAPSHOT.jar"
PORT="${SIDECAR_PORT:-8099}"

# 1) 确保 sidecar jar（手机只下载，不编译）。默认取本仓库最新 Release 资产，可用 SIDECAR_JAR_URL 覆盖。
JAR_URL="${SIDECAR_JAR_URL:-https://github.com/lehuaner/unified-novel-download/releases/latest/download/unidbg-boot-server.jar}"
if [ ! -f "$JAR" ]; then
  echo "[*] 下载 sidecar jar: $JAR_URL"
  mkdir -p "$HERE/target"
  if command -v curl >/dev/null 2>&1; then curl -fL --retry 3 -o "$JAR" "$JAR_URL"
  elif command -v wget >/dev/null 2>&1; then wget -O "$JAR" "$JAR_URL"
  else echo "[-] 需要 curl 或 wget"; exit 1; fi
fi

# 2) 选择 java
JAVA="${JAVA_BIN:-$(command -v java || true)}"
[ -n "$JAVA" ] || { echo "[-] 未找到 java（可设置 JAVA_BIN）"; exit 1; }

# 3) 启动（unidbg 在 Termux/Android 用 Unicorn 后端）
echo "[*] 启动 sidecar: port=$PORT"
exec "$JAVA" \
  -Xms64m -Xmx512m \
  -jar "$JAR" \
  --server.port="$PORT"
