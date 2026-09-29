#Requires -Version 5.1
<#
.SYNOPSIS
  桌面/CI 构建 sidecar jar（手机不需要做这一步，只下载产物）。
.DESCRIPTION
  产出：java-sidecar/target/unidbg-boot-server-0.0.1-SNAPSHOT.jar
  注意：该 jar 不内嵌 APK/.so；运行期由 TempFileUtils 从 sidecar-assets 解析。
  首次会由 mvnw 自动下载 Maven 与依赖（unidbg 0.9.8、Spring Boot 等）。
  JDK：pom 目标 1.8，但工程需 JDK 17/21 编译（lombok 1.18.34）。请确保 java -version >= 17。
#>
$ErrorActionPreference = 'Stop'
function Write-Err([string]$m) { Write-Host "[-] $m" -ForegroundColor Red }

# mvnw.cmd 强制要求 JAVA_HOME。若未设置，则从 PATH 上的 java 推导 JDK 目录。
if (-not $env:JAVA_HOME -or -not (Test-Path -LiteralPath $env:JAVA_HOME)) {
    $javaCmd = (Get-Command java -ErrorAction SilentlyContinue).Source
    if ($javaCmd) {
        $derived = Split-Path (Split-Path $javaCmd)   # .../jdk-xx/bin/java.exe -> .../jdk-xx
        if (Test-Path -LiteralPath (Join-Path $derived 'bin\javac.exe')) {
            $env:JAVA_HOME = $derived
            Write-Host "[*] 自动设置 JAVA_HOME=$derived" -ForegroundColor Yellow
        } else {
            Write-Err "未能从 java 推导 JDK 目录，请手动设置 JAVA_HOME"
        }
    }
}

$here = Split-Path -Parent $PSScriptRoot     # java-sidecar
Push-Location $here
try {
    if (Test-Path -LiteralPath "$here\mvnw.cmd") {
        & "$here\mvnw.cmd" -DskipTests clean package
    } else {
        & mvn -DskipTests clean package
    }
    if ($LASTEXITCODE -ne 0) { throw "Maven 构建失败 (exit $LASTEXITCODE)" }

    $jar = Join-Path $here 'target\unidbg-boot-server-0.0.1-SNAPSHOT.jar'
    if (Test-Path -LiteralPath $jar) {
        Write-Host "[+] 构建完成: $jar" -ForegroundColor Green
    } else {
        Write-Host "[-] 构建结束但未找到产物 jar: $jar" -ForegroundColor Red
        exit 1
    }
}
finally {
    Pop-Location
}
