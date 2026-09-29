#Requires -Version 5.1
<#
.SYNOPSIS
    One-click dev environment launcher for Unified Novel Downloader.
    Starts Java sidecar (unidbg) + Rust binary with watch mode.
    Both processes start/stop together. Ctrl+C kills both.

.DESCRIPTION
    1. Starts Java sidecar (unidbg-boot-server.jar) on port 8099
    2. Waits for sidecar health check to pass
    3. Starts Rust binary with cargo-watch (auto-rebuild on src/ changes)
    4. If either process exits, the other is killed and the script exits
    5. Ctrl+C cleanly stops both processes

.PARAMETER NoWatch
    Skip cargo-watch, just run cargo run directly (no auto-rebuild).

.PARAMETER NoClean
    Do NOT reap stale dev instances (leftover dev binary / sidecar JVM) before
    starting. By default they ARE reaped: a running dev binary holds an
    exclusive lock on target\debug\tomato-novel-downloader.exe, so the next
    build dies with "error: failed to remove file ...".

.EXAMPLE
    .\run-dev.ps1
    .\run-dev.ps1 -NoWatch
    run-dev.bat --no-watch --no-clean
#>

param(
    [switch]$NoWatch,
    [switch]$NoClean,
    [Parameter(ValueFromRemainingArguments = $true)]
    $CliArgs
)

# NOTE: `powershell -File run-dev.ps1 --no-watch` does NOT bind dash-style flags
# to a [switch] parameter - the token is silently dropped, so --no-watch used to
# be ignored and watch mode still started. Normalise CLI-style flags here.
if ($CliArgs) {
    foreach ($a in @($CliArgs)) {
        if ("$a" -match '^-{1,2}no-?watch$') {
            $NoWatch = $true
        } elseif ("$a" -match '^-{1,2}no-?clean$') {
            $NoClean = $true
        } elseif ("$a" -match '^(help|\?|-h|--help)$') {
            Write-Host "Usage: run-dev.bat [--no-watch] [--no-clean]" -ForegroundColor Cyan
            exit 0
        } else {
            Write-Host "[!] Ignored unknown argument: $a" -ForegroundColor Yellow
        }
    }
}

# ============================================================================
# Configuration - EDIT THESE VALUES
# ============================================================================

## Java sidecar JAR path (REQUIRED - set to your local path)
$JAR_PATH = "D:\Code\Project\Rust\fqnovel-unidbg\target\unidbg-boot-server-0.0.1-SNAPSHOT.jar"

## Java JVM options (unidbg needs more heap for native lib simulation)
$JAVA_OPTS = @(
    "-Xms64m", "-Xmx512m"
)

## Sidecar port (must match unidbg_signer_url in app config)
$SIDECAR_PORT = 8099

## Rust Web UI bind address
## 0.0.0.0 = 监听所有网卡，局域网设备可通过本机 IP（如 http://192.168.0.104:18423）访问。
## 若只需本机访问可改回 127.0.0.1:18423。首次对外访问需在 Windows 防火墙放行 TCP 18423 入站。
$WEB_ADDR = "0.0.0.0:18423"

## Rust Web UI password
$WEB_PASSWORD = "dev123456"

## Cargo features (comma-separated, no spaces)
## Use "no-official-api" if you don't have Tomato-Novel-Official-API repo locally.
## The workflow falls back to no-official-api when OAPI secret is not set.
$CARGO_FEATURES = "no-official-api,shuqi,qimao,tts,clipboard,clipboard-arboard"

## Disable default features (required when using no-official-api,
## otherwise default = ["official-api", ...] still enables official-api)
$NO_DEFAULT_FEATURES = $true

## Rust extra args passed to the binary
$RUST_ARGS = @("--server", "--debug")

## Binary name (without .exe). Used to locate and reap stale dev instances that
## lock target\debug\<bin>.exe and break cargo linking.
$BIN_NAME = "tomato-novel-downloader"

# ============================================================================
# End of configuration
# ============================================================================

$ErrorActionPreference = "Stop"
# NOTE: StrictMode 3.0 causes terminating errors when accessing .Source on $null
# (e.g. Get-Command returns $null when tool not in PATH). Use 2.0 instead.
Set-StrictMode -Version 2.0

# Always work from the repo root. `cargo run` builds/runs relative to the current
# directory, and run-dev.bat can be launched from an unrelated CWD (double-click,
# another folder), which would otherwise compile the wrong manifest.
Set-Location -LiteralPath $PSScriptRoot

$script:JavaProcess  = $null
$script:RustProcess  = $null
$script:Exiting      = $false
$script:NoClean      = [bool]$NoClean
$script:StartTime    = Get-Date
# NOTE: ${BIN_NAME} braces are required - "$BIN_NAME.exe" would be parsed as a
# property access and StrictMode 2.0 would abort the script.
$script:DebugBin     = Join-Path $PSScriptRoot "target\debug\${BIN_NAME}.exe"

function Write-Header([string]$msg) {
    Write-Host ""
    Write-Host $msg -ForegroundColor Cyan
    Write-Host ("=" * 60) -ForegroundColor DarkCyan
}

function Write-Step([string]$msg) {
    Write-Host "[*] $msg" -ForegroundColor Yellow
}

function Write-OK([string]$msg) {
    Write-Host "[+] $msg" -ForegroundColor Green
}

function Write-Err([string]$msg) {
    Write-Host "[-] $msg" -ForegroundColor Red
}

# ----------------------------------------------------------------------------
# Process / file-lock helpers
# ----------------------------------------------------------------------------

# Stop a process AND every descendant of it (post-order, deepest first).
# The tree is: cargo.exe -> cargo-watch.exe -> cargo.exe -> <bin>.exe
# Killing only the root orphaned the built binary, which then kept holding the
# file lock on target\debug\<bin>.exe and broke the next cargo build.
function Stop-ProcessTree {
    param(
        [int]$RootPid,
        [switch]$IncludeSelf
    )
    if ($RootPid -le 0) { return }
    $children = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$RootPid" -ErrorAction SilentlyContinue)
    foreach ($c in $children) {
        Stop-ProcessTree -RootPid ([int]$c.ProcessId) -IncludeSelf
    }
    if ($IncludeSelf) {
        try { Stop-Process -Id $RootPid -Force -ErrorAction SilentlyContinue } catch {}
    }
}

# Can we open the file for write with no sharing? Running .exe images are
# locked by the OS, so this fails while a dev instance is still alive.
function Test-FileUnlocked([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $true }
    try {
        $fs = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
        $fs.Dispose()
        return $true
    } catch {
        return $false
    }
}

function Wait-FilesUnlocked([string[]]$Paths, [int]$TimeoutSec = 20) {
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ($true) {
        $locked = @($Paths | Where-Object { -not (Test-FileUnlocked $_) })
        if ($locked.Count -eq 0) { return $true }
        if ((Get-Date) -gt $deadline) {
            foreach ($l in $locked) {
                Write-Err "Still locked: $l"
                @(Get-CimInstance Win32_Process -Filter "Name='${BIN_NAME}.exe'" -ErrorAction SilentlyContinue) |
                    ForEach-Object { Write-Host "    PID $($_.ProcessId)  $($_.ExecutablePath)" -ForegroundColor DarkGray }
            }
            return $false
        }
        Start-Sleep -Milliseconds 500
    }
}

# Stale dev processes owned by THIS project only (foreign tools are never
# touched). Scope is deliberately narrow:
#  - our DEBUG build output (cargo run/link overwrites exactly this file, so
#    only it can cause "failed to remove file"); a release instance the user
#    runs on purpose is left alone and surfaces as a port conflict instead
#  - a sidecar JVM started from $JAR_PATH
#  - an orphaned cargo-watch running OUR watch command (it would immediately
#    respawn the binary and fight the new instance over the same lock; matched
#    by the feature fingerprint so unrelated cargo builds are never touched)
# $OlderThan keeps a shutdown sweep from killing a newer instance that another
# terminal started legitimately after we began. Default MaxValue = "no limit"
# (everything is older than MaxValue); MinValue would exclude everything.
function Get-StaleProjectProcess([datetime]$OlderThan = [datetime]::MaxValue) {
    $dbg = $script:DebugBin.ToLowerInvariant()
    $rust = @(Get-CimInstance Win32_Process -Filter "Name='${BIN_NAME}.exe'" -ErrorAction SilentlyContinue | Where-Object {
        $ep = "$($_.ExecutablePath)".ToLowerInvariant()
        $cl = "$($_.CommandLine)".ToLowerInvariant()
        (($ep -eq $dbg) -or ($cl -like "*target\debug\${BIN_NAME}.exe*")) -and
        ([datetime]$_.CreationDate -lt $OlderThan)
    })
    $java = @(Get-CimInstance Win32_Process -Filter "Name='java.exe'" -ErrorAction SilentlyContinue | Where-Object {
        ("$($_.CommandLine)" -like "*$JAR_PATH*") -and ([datetime]$_.CreationDate -lt $OlderThan)
    })
    $watch = @(Get-CimInstance Win32_Process -Filter "Name='cargo-watch.exe'" -ErrorAction SilentlyContinue | Where-Object {
        (("$($_.CommandLine)") -like "*$CARGO_FEATURES*") -and ([datetime]$_.CreationDate -lt $OlderThan)
    })
    # cargo-watch is the parent of the built binary, so killing its tree covers
    # the binary too; keep both lists - Stop-ProcessTree tolerates already-gone PIDs.
    return @($rust) + @($java) + @($watch)
}

function Stop-StaleProjectProcesses([string]$Reason, [datetime]$OlderThan = [datetime]::MaxValue) {
    if ($script:NoClean) { return }
    $stale = @(Get-StaleProjectProcess $OlderThan)
    if ($stale.Count -eq 0) {
        if ($Reason -eq 'preflight') {
            Write-OK "No stale dev instances; build output is free."
        }
        return
    }
    Write-Step "$($stale.Count) stale dev instance(s) detected ($Reason):"
    foreach ($p in $stale) {
        $cmd = ('' + $p.CommandLine) -replace '\s+', ' '
        if ($cmd.Length -gt 110) { $cmd = $cmd.Substring(0, 110) + '...' }
        Write-Host "    PID $($p.ProcessId)  $($p.Name)  $cmd" -ForegroundColor DarkGray
        Stop-ProcessTree -RootPid ([int]$p.ProcessId) -IncludeSelf
    }
    Start-Sleep -Milliseconds 400
}

function Show-PortOwner([int]$Port) {
    try {
        $conns = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
    } catch { $conns = $null }
    if (-not $conns) { return }
    foreach ($procId in @($conns | Select-Object -ExpandProperty OwningProcess -Unique)) {
        $p = Get-CimInstance Win32_Process -Filter "ProcessId=$procId" -ErrorAction SilentlyContinue
        if ($p) {
            $cmd = ('' + $p.CommandLine) -replace '\s+', ' '
            if ($cmd.Length -gt 110) { $cmd = $cmd.Substring(0, 110) + '...' }
            Write-Host "    PID $procId  $($p.Name)  $cmd" -ForegroundColor DarkGray
        } else {
            Write-Host "    PID $procId" -ForegroundColor DarkGray
        }
    }
}

function Stop-AllProcesses {
    if ($script:Exiting) { return }
    $script:Exiting = $true

    Write-Header "Shutting down"

    # Kill Rust - the WHOLE tree, otherwise cargo-watch's child (the built
    # binary itself) survives as an orphan and locks target\debug\*.exe.
    if ($script:RustProcess -and -not $script:RustProcess.HasExited) {
        Write-Step "Stopping Rust process tree (root PID $($script:RustProcess.Id))..."
        Stop-ProcessTree -RootPid ([int]$script:RustProcess.Id) -IncludeSelf
        Write-OK "Rust stopped."
    }

    # Kill Java
    if ($script:JavaProcess -and -not $script:JavaProcess.HasExited) {
        Write-Step "Stopping Java sidecar tree (PID $($script:JavaProcess.Id))..."
        Stop-ProcessTree -RootPid ([int]$script:JavaProcess.Id) -IncludeSelf
        Write-OK "Java sidecar stopped."
    }

    # Sweep survivors of OUR tree (console-window close, Ctrl+C that only
    # reached the cargo parent). Instances started after this script began are
    # left alone - they belong to a different, newer session.
    Stop-StaleProjectProcesses 'shutdown' $script:StartTime

    Write-Host ""
    Write-Host "Dev environment stopped." -ForegroundColor Cyan
}

# Register cleanup on all exit paths
$null = Register-EngineEvent PowerShell.Exiting -Action { Stop-AllProcesses }
trap {
    Write-Err "TRAP: $($_.Exception.Message)"
    Write-Host "  at $($_.InvocationInfo.PositionMessage)" -ForegroundColor DarkRed
    Stop-AllProcesses
    break
}

# ============================================================================
# Step 0: Prerequisites check
# ============================================================================

Write-Header "Unified Novel Downloader - Dev Environment"

# Check Java
$javaCmd = Get-Command java -ErrorAction SilentlyContinue
$javaExe = if ($javaCmd) { $javaCmd.Source } else { $null }
if (-not $javaExe) {
    Write-Err "Java not found in PATH. Install JDK 21+ first."
    exit 1
}
$javaVersion = & { $ErrorActionPreference = "Continue"; (java -version 2>&1 | Select-Object -First 1) } -replace '"', ''
Write-OK "Java: $javaVersion ($javaExe)"

# Check JAR
if (-not (Test-Path $JAR_PATH)) {
    Write-Err "JAR not found: $JAR_PATH"
    Write-Host ""
    Write-Host "Edit run-dev.ps1 and set `$JAR_PATH to your unidbg-boot-server.jar location." -ForegroundColor Yellow
    exit 1
}
Write-OK "JAR: $JAR_PATH"

# Check cargo - Get-Command fails on non-ASCII PATH entries (e.g. Chinese username)
$cargoCmd = Get-Command cargo -ErrorAction SilentlyContinue
$cargoExe = if ($cargoCmd) { $cargoCmd.Source } else { $null }
if (-not $cargoExe) {
    # Fallback: check common cargo locations
    $cargoFallback = @(
        "$env:USERPROFILE\.cargo\bin\cargo.exe"
        "$env:CARGO_HOME\bin\cargo.exe"
    ) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
    if ($cargoFallback) { $cargoExe = $cargoFallback }
}
if (-not $cargoExe) {
    Write-Err "cargo not found in PATH or ~/.cargo/bin. Install Rust first."
    exit 1
}
Write-OK "Cargo: $cargoExe"

# Check/install cargo-watch
$useWatch = -not $NoWatch
if ($useWatch) {
    $watchCmd = Get-Command cargo-watch -ErrorAction SilentlyContinue
    $cargoWatch = if ($watchCmd) { $watchCmd.Source } else { $null }
    if (-not $cargoWatch) {
        $watchFallback = @(
            "$env:USERPROFILE\.cargo\bin\cargo-watch.exe"
            "$env:CARGO_HOME\bin\cargo-watch.exe"
        ) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
        if ($watchFallback) { $cargoWatch = $watchFallback }
    }
    if (-not $cargoWatch) {
        Write-Step "cargo-watch not found. Installing..."
        & $cargoExe install cargo-watch 2>&1 | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }
        $cargoWatch = @(
            "$env:USERPROFILE\.cargo\bin\cargo-watch.exe"
            "$env:CARGO_HOME\bin\cargo-watch.exe"
        ) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
        if (-not $cargoWatch) {
            Write-Err "Failed to install cargo-watch. Falling back to no-watch mode."
            $useWatch = $false
        } else {
            Write-OK "cargo-watch installed."
        }
    } else {
        Write-OK "cargo-watch: $cargoWatch"
    }
}

# ============================================================================
# Step 0.3: Reap stale dev instances (fixes "failed to remove file ...exe")
# ============================================================================
# Windows locks a running .exe, so a leftover dev binary (orphaned cargo-watch
# child, IDE debug launch, manual `cargo run`) makes cargo die with:
#   error: failed to remove file `...\target\debug\tomato-novel-downloader.exe`
# Reap it (and stale sidecar JVMs) before building, then wait for the OS to
# release the file handle.
$webPort = ($WEB_ADDR -split ':')[1]
if ($NoClean) {
    Write-Step "Stale-instance cleanup skipped (--no-clean)."
} else {
    Stop-StaleProjectProcesses 'preflight'
    if (-not (Wait-FilesUnlocked @($script:DebugBin) 20)) {
        Write-Err "Build output is still locked - cargo would fail to link again."
        Write-Host "Kill the holder manually, then re-run:" -ForegroundColor Yellow
        Write-Host "    Get-Process '${BIN_NAME}' | Select-Object Id,Path" -ForegroundColor Yellow
        exit 1
    }
}

# Check if sidecar port is already in use (anything left here = foreign process)
try {
    $portInUse = Get-NetTCPConnection -LocalPort $SIDECAR_PORT -State Listen -ErrorAction SilentlyContinue
} catch { $portInUse = $null }
if ($portInUse) {
    Write-Err "Port $SIDECAR_PORT is already in use by a process this script does not own:"
    Show-PortOwner $SIDECAR_PORT
    exit 1
}

# Check if web port is already in use
try {
    $webPortInUse = Get-NetTCPConnection -LocalPort ([int]$webPort) -State Listen -ErrorAction SilentlyContinue
} catch { $webPortInUse = $null }
if ($webPortInUse) {
    Write-Err "Port $webPort is already in use by a process this script does not own:"
    Show-PortOwner ([int]$webPort)
    exit 1
}

# ============================================================================
# Step 0.5: LAN firewall rule (best-effort, only when binding non-loopback)
# ============================================================================
$webHost = ($WEB_ADDR -split ':')[0]
if ($webHost -notin @('127.0.0.1', 'localhost', '::1')) {
    $fwRuleName = "UnifiedNovelDownloader Web ($webPort)"
    $fwExisting = Get-NetFirewallRule -DisplayName $fwRuleName -ErrorAction SilentlyContinue
    if (-not $fwExisting) {
        Write-Step "Adding Windows Firewall inbound rule for TCP $webPort (LAN access)..."
        try {
            New-NetFirewallRule -DisplayName $fwRuleName -Direction Inbound -Protocol TCP -LocalPort $webPort -Action Allow -Profile Any -ErrorAction Stop | Out-Null
            Write-OK "Firewall rule added."
        } catch {
            Write-Step "Could not add firewall rule automatically (needs admin). To allow LAN devices, run PowerShell as Administrator and execute:"
            Write-Host "    New-NetFirewallRule -DisplayName `"$fwRuleName`" -Direction Inbound -Protocol TCP -LocalPort $webPort -Action Allow" -ForegroundColor Yellow
        }
    } else {
        Write-OK "Firewall rule already present: $fwRuleName"
    }
}

# ============================================================================
# Step 1: Start Java sidecar
# ============================================================================

Write-Header "Starting Java Sidecar (port $SIDECAR_PORT)"

$javaArgs = @($JAVA_OPTS) + @("-jar", $JAR_PATH)
Write-Step "Command: java $($javaArgs -join ' ')"

$script:JavaProcess = Start-Process -FilePath $javaExe `
    -ArgumentList $javaArgs `
    -PassThru `
    -NoNewWindow `
    -RedirectStandardOutput "java-sidecar.log" `
    -RedirectStandardError  "java-sidecar-err.log"

Write-OK "Java sidecar started (PID $($script:JavaProcess.Id))"

# Wait for health check
Write-Step "Waiting for sidecar to be healthy..."
$healthy = $false
for ($i = 1; $i -le 60; $i++) {
    Start-Sleep -Milliseconds 1000

    # Check if process died
    if ($script:JavaProcess.HasExited) {
        Write-Err "Java sidecar exited prematurely (code $($script:JavaProcess.ExitCode))."
        Write-Host "Check java-sidecar-err.log for details." -ForegroundColor Yellow
        Stop-AllProcesses
        exit 1
    }

    try {
        $response = Invoke-WebRequest -Uri "http://127.0.0.1:$SIDECAR_PORT/" `
            -TimeoutSec 2 -UseBasicParsing -ErrorAction Stop
        $healthy = $true
        break
    } catch {
        # A 4xx/5xx HTTP response means the server IS running (just no root endpoint)
        if ($_.Exception.Response) {
            $healthy = $true
            break
        }
        # Connection refused = still starting up
        if ($i % 5 -eq 0) {
            Write-Host "    ...waiting ($i s)" -ForegroundColor DarkGray
        }
    }
}

if (-not $healthy) {
    Write-Err "Sidecar did not become healthy within 60s."
    Stop-AllProcesses
    exit 1
}

Write-OK "Sidecar healthy after $i s."
Write-Host "    Signature endpoint: http://127.0.0.1:$SIDECAR_PORT/api/fq-signature/generateSignatureWithMap" -ForegroundColor DarkGray

# ============================================================================
# Step 2: Start Rust binary (with watch)
# ============================================================================

Write-Header "Starting Rust Binary (port $WEB_ADDR)"

# Set environment variables
$env:TOMATO_WEB_ADDR     = $WEB_ADDR
$env:TOMATO_WEB_PASSWORD = $WEB_PASSWORD
$env:RUST_LOG            = "debug"

$featureFlag   = if ($CARGO_FEATURES) { "--features", $CARGO_FEATURES } else { @() }
$noDefaultFlag = if ($NO_DEFAULT_FEATURES) { "--no-default-features" } else { @() }
$rustArgStr    = $RUST_ARGS -join ' '

if ($useWatch) {
    # cargo watch -x 'run --no-default-features --features ... -- --server --debug'
    # NOTE: Start-Process -ArgumentList joins array elements with spaces.
    #       The -x value contains spaces, so it MUST be wrapped in escaped quotes,
    #       otherwise cargo watch only sees "-x run" and tries to execute
    #       "--server --debug" as a shell command.
    # NOTE: restrict the watcher to source only (-w src -w Cargo.toml -w build.rs).
    #       The server writes runtime files into the repo (logs/, config.yml,
    #       download_history.jsonl, web_session_secret.key); watching the whole tree
    #       triggers a rebuild/restart storm that interrupts in-flight requests
    #       (browser "Failed to fetch"). Watching only src keeps hot-reload working.
    $watchCommand = "run $($noDefaultFlag) $($featureFlag -join ' ') -- $rustArgStr"
    Write-Step "Command: cargo watch -x `"$watchCommand`""
    Write-Step "Watching src/ for changes... (Ctrl+C to stop both)"

    $script:RustProcess = Start-Process -FilePath $cargoExe `
        -ArgumentList "watch -w src -w Cargo.toml -w build.rs -x `"$watchCommand`"" `
        -PassThru `
        -NoNewWindow
} else {
    # Plain cargo run.
    # NOTE: use array concatenation, NOT @("run", $noDefaultFlag, $featureFlag, ...).
    #       The @() literal keeps $noDefaultFlag/$featureFlag as NESTED arrays, and
    #       Start-Process then renders them literally as "System.Object[]", so cargo
    #       was handed garbage arguments.
    $runArgs = @("run") + $noDefaultFlag + $featureFlag + @("--") + $RUST_ARGS
    Write-Step "Command: cargo $($runArgs -join ' ')"
    Write-Step "No watch mode. Ctrl+C to stop both."

    $script:RustProcess = Start-Process -FilePath $cargoExe `
        -ArgumentList $runArgs `
        -PassThru `
        -NoNewWindow
}

Write-OK "Rust process started (PID $($script:RustProcess.Id))"

# ============================================================================
# Step 3: Monitor - if either dies, kill both
# ============================================================================

Write-Header "Running"
Write-Host "  Web UI:      http://$WEB_ADDR" -ForegroundColor White
Write-Host "  Password:    $WEB_PASSWORD" -ForegroundColor White
Write-Host "  Sidecar:     http://127.0.0.1:$SIDECAR_PORT" -ForegroundColor White
Write-Host "  Java log:    java-sidecar.log" -ForegroundColor DarkGray
Write-Host "  Java stderr: java-sidecar-err.log" -ForegroundColor DarkGray
Write-Host ""
Write-Host "  Press Ctrl+C to stop both processes." -ForegroundColor Yellow
Write-Host ""

# Monitor loop: check both processes every 2s
try {
    while (-not $script:Exiting) {
        Start-Sleep -Seconds 2

        if ($script:JavaProcess -and $script:JavaProcess.HasExited) {
            Write-Err "Java sidecar exited (code $($script:JavaProcess.ExitCode))."
            break
        }

        if ($script:RustProcess -and $script:RustProcess.HasExited) {
            Write-Err "Rust process exited (code $($script:RustProcess.ExitCode))."
            break
        }
    }
} finally {
    Stop-AllProcesses
}
