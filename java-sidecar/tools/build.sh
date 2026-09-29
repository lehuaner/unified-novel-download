#!/usr/bin/env bash
# 桌面/CI 构建 sidecar jar（手机不做这步，只下载产物）。
# 产出：java-sidecar/target/unidbg-boot-server-0.0.1-SNAPSHOT.jar
# 该 jar 不内嵌 APK/.so；运行期由 TempFileUtils 从 sidecar-assets 解析。
# 需 JDK 17/21（lombok 1.18.34）；首次 mvnw 会自动下载 Maven 与依赖。
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"

if [ -x "./mvnw" ]; then
  chmod +x ./mvnw 2>/dev/null || true
  ./mvnw -DskipTests clean package
else
  mvn -DskipTests clean package
fi

JAR="$HERE/target/unidbg-boot-server-0.0.1-SNAPSHOT.jar"
if [ -f "$JAR" ]; then
  echo "[+] 构建完成: $JAR"
else
  echo "[-] 构建结束但未找到产物 jar: $JAR"; exit 1
fi
