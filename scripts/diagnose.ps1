<#
.SYNOPSIS
    Lightweight, reproducible health and performance diagnostic runner for VIctory.
.DESCRIPTION
    Checks local hardware (CPU, RAM, GPU, Desktop/Laptop), verifies title import
    coverage via `victory census`, runs a controlled diagnostic session (or
    interactive session), monitors RAM / Commit / VRAM / GPU utilization &
    temperature, and summarizes shader/pipeline/GPU/CPU health with a PASS/WARN/FAIL
    verdict. All outputs are written outside the repository under %TEMP%.
.EXAMPLE
    .\scripts\diagnose.ps1
.EXAMPLE
    .\scripts\diagnose.ps1 -Seconds 120 -WaveWide last -Keys "25:Enter,33:X,42:Enter,50:X"
.EXAMPLE
    .\scripts\diagnose.ps1 -Interactive -Overlay
#>
param(
    [string]$Exe,
    [int]$Seconds = 95,
    [switch]$Interactive,
    [switch]$Safe,
    [switch]$NoPorts,
    [ValidateSet('off', 'last', '1')][string]$WaveWide = 'last',
    [ValidateSet('series-x', 'series-s')][string]$Console = 'series-x',
    [ValidateSet('quality', 'balanced', 'performance')][string]$Fsr4K,
    [ValidateSet('diag', 'release')][string]$Build = 'diag',
    [switch]$Overlay,
    [string]$Keys = '25:Enter,33:X,41:Enter,49:X,57:Enter',
    [int]$SampleInterval = 5,
    [hashtable]$More = @{}
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$victory = Join-Path $repoRoot "target\$Build\victory.exe"
if (-not (Test-Path $victory)) {
    Write-Error "Missing $victory. Build first with: cargo build --profile $Build -p victory --features diagnostics"
    exit 1
}

# Resolve title executable from victory.toml if -Exe was not provided.
if (-not $Exe) {
    $tomlPath = Join-Path $repoRoot 'victory.toml'
    if (Test-Path $tomlPath) {
        $match = Select-String -Path $tomlPath -Pattern "game_dir\s*=\s*['""](.+?)['""]" | Select-Object -First 1
        if ($match) {
            $candidate = Join-Path $match.Matches[0].Groups[1].Value 'game_scarlett_master.exe'
            if (Test-Path $candidate) { $Exe = $candidate }
        }
    }
}
if (-not $Exe -or -not (Test-Path $Exe)) {
    Write-Error "Title executable not found. Pass -Exe <path-to-game_scarlett_master.exe> or set game_dir in victory.toml."
    exit 1
}
$Exe = (Resolve-Path $Exe).Path

# Output directory strictly outside the repository.
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$outDir = Join-Path $env:TEMP "victory-diag-$stamp"
New-Item -ItemType Directory -Path $outDir -Force | Out-Null
$stdoutPath = Join-Path $outDir 'stdout.log'
$stderrPath = Join-Path $outDir 'stderr.log'
$monPath    = Join-Path $outDir 'monitor.csv'
$reportJson = Join-Path $outDir 'report.json'

Write-Host "=== VIctory Diagnostic Runner ===" -ForegroundColor Cyan
Write-Host "Repo   : $repoRoot"
Write-Host "Binary : $victory"
Write-Host "Title  : $Exe"
Write-Host "Output : $outDir"

# 1. Hardware snapshot
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$os  = Get-CimInstance Win32_OperatingSystem
$bat = Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue
$totalRamGiB = [math]::Round($os.TotalVisibleMemorySize / 1MB, 1)
$freeRamGiB  = [math]::Round($os.FreePhysicalMemory / 1MB, 1)
$freeCommitGiB = [math]::Round($os.FreeVirtualMemory / 1MB, 1)
$isLaptop = $null -ne $bat
$hasNvidiaSmi = $null -ne (Get-Command nvidia-smi -ErrorAction SilentlyContinue)
$gpuSummary = if ($hasNvidiaSmi) {
    (& nvidia-smi --query-gpu=name,memory.total,driver_version --format=csv,noheader 2>$null) -join ' | '
} else {
    ((Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name }) -join ' | ')
}

Write-Host ("CPU    : {0} ({1}C/{2}T)" -f $cpu.Name.Trim(), $cpu.NumberOfCores, $cpu.NumberOfLogicalProcessors)
Write-Host ("RAM    : {0} GiB total, {1} GiB free, {2} GiB commit free | Form: {3}" -f $totalRamGiB, $freeRamGiB, $freeCommitGiB, $(if ($isLaptop) { 'Laptop' } else { 'Desktop' }))
Write-Host ("GPU    : {0}" -f $gpuSummary)

# 2. Bind-only import preflight (fast, resolves HLE + host + title DLLs)
Write-Host "`n[1/3] Running bind-only import preflight..." -ForegroundColor Yellow
$prevEap = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$bindOut = & $victory run --console $Console $Exe 2>$null
$bindCode = $LASTEXITCODE
$ErrorActionPreference = $prevEap
$bindText = ($bindOut | Out-String)
$boundLines = [regex]::Matches($bindText, 'bound\s+(\S+):\s+(\d+)\s+hle,\s+(\d+)\s+host,\s+(\d+)\s+title,\s+(\d+)\s+missing')
$missingImports = 0
if ($bindCode -ne 0 -or $boundLines.Count -eq 0) {
    $missingImports = -1
    Write-Host "Bind   : failed or could not parse bind output (exit=$bindCode)" -ForegroundColor Yellow
} else {
    foreach ($m in $boundLines) {
        $missingImports += [int]$m.Groups[5].Value
    }
    Write-Host ("Bind   : {0} images bound, {1} missing imports" -f $boundLines.Count, $missingImports)
}

# 3. Environment setup (mirrors scripts/play.ps1 cleanly)
$settings = @{
    VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_RELAY = '2'; VICTORY_WIDE_BACK = '1'; VICTORY_CARD_REHASH_RUNS = '1'
    VICTORY_JOIN_COPIES = '1'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_RELAY_COPY = '1'; VICTORY_CARD_STEPS = '2'
    VICTORY_INDIRECT_BATCH = '2'; VICTORY_MAP_THREADS = '1'; VICTORY_TITLE_PRIORITIES = '0'; VICTORY_CARD_AWAKE = 'auto'
    VICTORY_SHOW_AFTER = '1'; VICTORY_AMBIENT = '0.8'
}
if ($Build -eq 'diag') { $settings.VICTORY_D3D12_DRED = 'light' }
if ($Safe) {
    $settings.VICTORY_UNDER_WIDE = '0'; $settings.VICTORY_LAND_ECHO = '0'
} else {
    $settings.VICTORY_THIN = '1'; $settings.VICTORY_JOIN_PARTS = '8'; $settings.VICTORY_UPLOAD_AHEAD = '1'
}
if (-not $NoPorts -and -not $Safe) {
    $settings.VICTORY_PORT_CONSTANTS = '1'; $settings.VICTORY_PORT_DESCRIPTORS = '1'; $settings.VICTORY_PORT_CBV = '1'
    $settings.VICTORY_PORT_GUARD = '1'; $settings.VICTORY_PORT_JOURNAL = '1'; $settings.VICTORY_SETTLE_ROOM = '1'
}
if ($WaveWide -ne 'off') { $settings.VICTORY_WAVE_WIDE = $WaveWide }
if ($Fsr4K) { $settings.VICTORY_FSR_4K = $Fsr4K.ToLowerInvariant() }
if ($Overlay) { $settings.VICTORY_OVERLAY = 'show' }
if (-not $Interactive -and $Keys) { $settings.VICTORY_KEYS = $Keys }
foreach ($k in $More.Keys) { $settings[$k] = [string]$More[$k] }

# Save & clean inherited VICTORY_* vars (preserving path overrides)
$keepPaths = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR', 'VICTORY_FIDELITYFX_DIR'
$savedEnv = @{}
foreach ($e in (Get-ChildItem Env:VICTORY_*)) {
    $savedEnv[$e.Name] = $e.Value
    if ($e.Name -notin $keepPaths) { Remove-Item "Env:$($e.Name)" }
}
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }

Write-Host "`n[2/3] Launching title ($(if ($Interactive) { 'interactive - close window when done' } else { "${Seconds}s timed run" }), console=$Console, wave_wide=$WaveWide)..." -ForegroundColor Yellow

if ($Interactive) {
    Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class VictoryWinShow {
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int X, int Y, int cx, int cy, uint uFlags);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr hWnd, StringBuilder lpClassName, int nMaxCount);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    private const uint SWP_NOSIZE = 0x0001;
    private const uint SWP_NOMOVE = 0x0002;
    private const uint SWP_SHOWWINDOW = 0x0040;
    public static bool Reveal(int targetPid) {
        bool shown = false;
        var cls = new StringBuilder(64);
        EnumWindows((h, _) => {
            uint pid;
            GetWindowThreadProcessId(h, out pid);
            if (pid == (uint)targetPid) {
                cls.Clear();
                GetClassName(h, cls, cls.Capacity);
                string c = cls.ToString();
                if (c == "ConsoleWindowClass" || c == "PseudoConsoleWindow" || c == "IME" || c == "MSCTFIME UI") {
                    return true;
                }
                RECT r;
                if (GetWindowRect(h, out r) && (r.Right - r.Left) >= 300 && (r.Bottom - r.Top) >= 200) {
                    // Two ShowWindow calls ensure Windows consumes any inherited STARTF_USESHOWWINDOW=SW_HIDE.
                    ShowWindow(h, 9); // SW_RESTORE
                    ShowWindow(h, 5); // SW_SHOW
                    SetWindowPos(h, IntPtr.Zero, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
                    BringWindowToTop(h);
                    SetForegroundWindow(h);
                    if (IsWindowVisible(h) && !IsIconic(h)) {
                        shown = true;
                    }
                }
            }
            return true;
        }, IntPtr.Zero);
        return shown;
    }
}
"@ -ErrorAction SilentlyContinue
}

$argsList = @('run', '--console', $Console, $Exe, '--execute')
$samples = [System.Collections.Generic.List[object]]::new()
"elapsed_s,ws_mib,private_mib,free_ram_mib,free_commit_mib,vram_used_mib,vram_total_mib,gpu_util_pct,gpu_temp_c" | Set-Content $monPath

$exitedEarly = $false
$exitCode = 0
$revealed = $false
$sw = [Diagnostics.Stopwatch]::StartNew()
$nextSampleAt = [double]$SampleInterval
try {
    # In -Interactive mode use -WindowStyle Normal (instead of -NoNewWindow) so
    # Windows does not propagate SW_HIDE to the title's first ShowWindow call
    # when launched from a hidden/non-interactive parent shell.
    $startParams = @{
        FilePath               = $victory
        ArgumentList           = $argsList
        WorkingDirectory       = $repoRoot
        RedirectStandardOutput = $stdoutPath
        RedirectStandardError  = $stderrPath
        PassThru               = $true
    }
    if ($Interactive) {
        $startParams.WindowStyle = 'Normal'
    } else {
        $startParams.NoNewWindow = $true
    }
    $proc = Start-Process @startParams

    while (-not $proc.HasExited) {
        if (-not $Interactive -and $sw.Elapsed.TotalSeconds -ge $Seconds) { break }
        if ($Interactive -and -not $revealed -and $sw.Elapsed.TotalSeconds -lt 30) {
            Start-Sleep -Milliseconds 500
            if (-not $proc.HasExited) { $revealed = [VictoryWinShow]::Reveal($proc.Id) }
            if ($sw.Elapsed.TotalSeconds -lt $nextSampleAt) { continue }
        } else {
            Start-Sleep -Seconds $SampleInterval
        }
        if ($proc.HasExited) { break }
        $nextSampleAt = $sw.Elapsed.TotalSeconds + $SampleInterval
        $pInfo = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
        if (-not $pInfo) { break }
        $wsMiB = [math]::Round($pInfo.WorkingSet64 / 1MB)
        $pmMiB = [math]::Round($pInfo.PrivateMemorySize64 / 1MB)
        $osNow = Get-CimInstance Win32_OperatingSystem
        $freeRamMiB = [math]::Round($osNow.FreePhysicalMemory / 1024)
        $freeCommitMiB = [math]::Round($osNow.FreeVirtualMemory / 1024)
        $vUsed = $null; $vTot = $null; $gUtil = $null; $gTemp = $null
        if ($hasNvidiaSmi) {
            $nv = (& nvidia-smi --query-gpu=memory.used,memory.total,utilization.gpu,temperature.gpu --format=csv,noheader,nounits 2>$null | Select-Object -First 1)
            if ($nv) {
                $parts = $nv -split ',\s*'
                if ($parts.Count -ge 4) {
                    $vUsed = [int]$parts[0]; $vTot = [int]$parts[1]; $gUtil = [int]$parts[2]; $gTemp = [int]$parts[3]
                }
            }
        }
        $t = [math]::Round($sw.Elapsed.TotalSeconds, 1)
        $row = [pscustomobject]@{
            elapsed_s = $t; ws_mib = $wsMiB; private_mib = $pmMiB
            free_ram_mib = $freeRamMiB; free_commit_mib = $freeCommitMiB
            vram_used_mib = $vUsed; vram_total_mib = $vTot; gpu_util_pct = $gUtil; gpu_temp_c = $gTemp
        }
        $samples.Add($row) | Out-Null
        "$t,$wsMiB,$pmMiB,$freeRamMiB,$freeCommitMiB,$vUsed,$vTot,$gUtil,$gTemp" | Add-Content $monPath
    }
    $exitedEarly = $proc.HasExited
    if ($exitedEarly) {
        $exitCode = $proc.ExitCode
    } else {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        $exitCode = 0
    }
} finally {
    # Restore caller's VICTORY_* environment
    foreach ($e in (Get-ChildItem Env:VICTORY_*)) {
        if ($e.Name -notin $keepPaths) { Remove-Item "Env:$($e.Name)" }
    }
    foreach ($k in $savedEnv.Keys) { Set-Item "Env:$k" $savedEnv[$k] }
}
$elapsed = [math]::Round($sw.Elapsed.TotalSeconds, 1)

# 4. Parse logs and summarize
Write-Host "`n[3/3] Analyzing diagnostic logs..." -ForegroundColor Yellow
$errLines = if (Test-Path $stderrPath) { Get-Content $stderrPath } else { @() }
$cleanErr = $errLines | ForEach-Object { $_ -replace '\x1b\[[0-9;]*m', '' }

$adapterLine = ($cleanErr | Select-String 'host D3D12 adapter' | Select-Object -First 1).Line
$waveLine    = ($cleanErr | Select-String 'host wave lanes' | Select-Object -First 1).Line
$resLine     = ($cleanErr | Select-String 'enabling VICTORY_RESIDENCY' | Select-Object -First 1).Line
$awakeLine   = ($cleanErr | Select-String 'VICTORY_CARD_AWAKE' | Select-Object -First 1).Line
$port2Lines  = @($cleanErr | Select-String 'renderer port 2:.*patched')

$refusedLines = @($cleanErr | Select-String -Pattern 'pipeline.*refused|refused.*pipeline|CreateComputePipelineState failed|CreateGraphicsPipelineState failed')
$deviceLost   = @($cleanErr | Select-String -Pattern 'device lost|DXGI_ERROR_DEVICE_REMOVED|DXGI_ERROR_DEVICE_HUNG|panic')
$pressureHits = @($cleanErr | Select-String -Pattern 'host memory pressure:')
$errorLines   = @($cleanErr | Select-String -Pattern '\sERROR\s')
$warnLines    = @($cleanErr | Select-String -Pattern '\sWARN\s')
$port2Missed  = @($cleanErr | Select-String -Pattern 'renderer port 2: a view record in the title''s heap that no store told of')
$occludedHits = @($cleanErr | Select-String -Pattern 'HRESULT\(0x087A0001\)')
$sse4aLines   = @($cleanErr | Select-String -Pattern 'console CPU instructions that fault')
$frameLines   = @($cleanErr | Select-String -Pattern 'frame presented')
$fpsLines     = @($cleanErr | Select-String -Pattern 'draws/frame')
$cacheLines   = @($cleanErr | Select-String -Pattern 'shaders from the cache')
$vramLines    = @($cleanErr | Select-String -Pattern 'video memory as the system counts it')

$peakWs     = ($samples | Measure-Object -Property ws_mib -Maximum).Maximum
$peakPriv   = ($samples | Measure-Object -Property private_mib -Maximum).Maximum
$minFreeRam = ($samples | Measure-Object -Property free_ram_mib -Minimum).Minimum
$minCommit  = ($samples | Measure-Object -Property free_commit_mib -Minimum).Minimum
$peakVram   = ($samples | Where-Object { $null -ne $_.vram_used_mib } | Measure-Object -Property vram_used_mib -Maximum).Maximum
$peakUtil   = ($samples | Where-Object { $null -ne $_.gpu_util_pct } | Measure-Object -Property gpu_util_pct -Maximum).Maximum
$peakTemp   = ($samples | Where-Object { $null -ne $_.gpu_temp_c } | Measure-Object -Property gpu_temp_c -Maximum).Maximum

$verdict = 'PASS'
$reasons = [System.Collections.Generic.List[string]]::new()
if ($missingImports -gt 0) { $verdict = 'FAIL'; $reasons.Add("$missingImports missing imports") }
if ($exitedEarly -and -not $Interactive -and $exitCode -ne 0) { $verdict = 'FAIL'; $reasons.Add("exited early with code $exitCode") }
if ($deviceLost.Count -gt 0) { $verdict = 'FAIL'; $reasons.Add("device lost/panic ($($deviceLost.Count))") }
if ($errorLines.Count -gt 0) { $verdict = 'FAIL'; $reasons.Add("$($errorLines.Count) ERROR log lines") }
if ($verdict -eq 'PASS') {
    if ($refusedLines.Count -gt 0) { $verdict = 'WARN'; $reasons.Add("$($refusedLines.Count) refused pipelines") }
    if ($pressureHits.Count -gt 0) { $verdict = 'WARN'; $reasons.Add("memory pressure triggered ($($pressureHits.Count)x)") }
    if ($null -ne $peakTemp -and $peakTemp -ge 85) { $verdict = 'WARN'; $reasons.Add("high GPU temp (${peakTemp}C)") }
}

$summary = [ordered]@{
    verdict = $verdict
    reasons = $reasons
    elapsed_s = $elapsed
    exited_early = $exitedEarly
    exit_code = $exitCode
    console = $Console
    wave_wide = $WaveWide
    missing_imports = $missingImports
    adapter = $adapterLine
    wave_lanes = $waveLine
    residency = $resLine
    card_awake = $awakeLine
    port2_patched = $port2Lines.Count
    peak_ws_mib = $peakWs
    peak_private_mib = $peakPriv
    min_free_ram_mib = $minFreeRam
    min_free_commit_mib = $minCommit
    peak_vram_mib = $peakVram
    peak_gpu_util_pct = $peakUtil
    peak_gpu_temp_c = $peakTemp
    frames_logged = ($frameLines | Select-Object -Last 1).Line
    fps_last = ($fpsLines | Select-Object -Last 1).Line
    shader_cache_last = ($cacheLines | Select-Object -Last 1).Line
    sse4a_last = ($sse4aLines | Select-Object -Last 1).Line
    refused_count = $refusedLines.Count
    refused_samples = @($refusedLines | Select-Object -First 10 | ForEach-Object { $_.Line })
    memory_pressure_count = $pressureHits.Count
    error_count = $errorLines.Count
    warn_count = $warnLines.Count
    port2_missed_count = $port2Missed.Count
    occluded_count = $occludedHits.Count
    window_revealed = $revealed
    output_dir = $outDir
}
$summary | ConvertTo-Json -Depth 4 | Set-Content $reportJson

$color = switch ($verdict) { 'PASS' { 'Green' } 'WARN' { 'Yellow' } default { 'Red' } }
Write-Host "`n================ DIAGNOSTIC SUMMARY ================" -ForegroundColor Cyan
Write-Host ("Verdict        : {0}{1}" -f $verdict, $(if ($reasons.Count) { " ($($reasons -join '; '))" } else { '' })) -ForegroundColor $color
Write-Host ("Elapsed        : {0}s (exitedEarly={1}, code={2})" -f $elapsed, $exitedEarly, $exitCode)
Write-Host ("RAM Peak       : WS={0} MiB | Private={1} MiB | MinFreeRAM={2} MiB | MinCommit={3} MiB" -f $peakWs, $peakPriv, $minFreeRam, $minCommit)
if ($null -ne $peakVram) {
    Write-Host ("GPU Peak       : VRAM={0} MiB | Util={1}% | Temp={2}C" -f $peakVram, $peakUtil, $peakTemp)
}
Write-Host ("Port 2 Patched : {0} sites (missed={1}) | Refused: {2} | Errors: {3} | Warns: {4} | Occluded: {5} | Pressure: {6}" -f $port2Lines.Count, $port2Missed.Count, $refusedLines.Count, $errorLines.Count, $warnLines.Count, $occludedHits.Count, $pressureHits.Count)
if ($summary.frames_logged)     { Write-Host ("Last Frame     : {0}" -f $summary.frames_logged) }
if ($summary.fps_last)          { Write-Host ("Last FPS/World : {0}" -f $summary.fps_last) }
if ($summary.shader_cache_last) { Write-Host ("Shader Cache   : {0}" -f $summary.shader_cache_last) }
if ($summary.sse4a_last)        { Write-Host ("Console Insns  : {0}" -f $summary.sse4a_last) }
if ($refusedLines.Count -gt 0) {
    Write-Host "Refused Pipelines:" -ForegroundColor Yellow
    $refusedLines | Select-Object -First 10 | ForEach-Object { Write-Host "  $($_.Line)" }
}
Write-Host ("Logs & JSON    : {0}" -f $outDir)
Write-Host "====================================================" -ForegroundColor Cyan
