param(
    [string]$ShmPath = "winshm://Local/gst-shm2-nv12-attach-detach-$PID",
    [int]$TotalSeconds = 24,
    [int]$AttachSeconds = 4,
    [int]$DetachSeconds = 2,
    [UInt64]$ShmSizeBytes = 67108864,
    [switch]$SkipBuild,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..")
$env:GST_PLUGIN_PATH = Join-Path $root "target\debug"

$sinkLogOut = Join-Path $env:TEMP ("shm2sink-winshm-out-{0}.log" -f ([Guid]::NewGuid().ToString("N")))
$sinkLogErr = Join-Path $env:TEMP ("shm2sink-winshm-err-{0}.log" -f ([Guid]::NewGuid().ToString("N")))
$srcLogOut = Join-Path $env:TEMP ("shm2src-winshm-out-{0}.log" -f ([Guid]::NewGuid().ToString("N")))
$srcLogErr = Join-Path $env:TEMP ("shm2src-winshm-err-{0}.log" -f ([Guid]::NewGuid().ToString("N")))

$sinkProc = $null
$srcProc = $null

function Assert-ProcessAlive {
    param(
        [Parameter(Mandatory = $true)]
        [System.Diagnostics.Process]$Process,
        [Parameter(Mandatory = $true)]
        [string]$Name
    )
    if ($Process.HasExited) {
        throw "FAIL: $Name process exited unexpectedly"
    }
}

function Stop-ProcessIfRunning {
    param([System.Diagnostics.Process]$Process)
    if ($null -ne $Process -and -not $Process.HasExited) {
        Stop-Process -Id $Process.Id -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 150
    }
}

try {
    if (-not $DryRun) {
        Get-Command gst-launch-1.0 -ErrorAction Stop | Out-Null
    }

    if (-not $SkipBuild -and -not $DryRun) {
        Write-Host "Building plugin..."
        & cargo build --lib | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --lib failed with exit code $LASTEXITCODE"
        }
    }

    Write-Host "Starting sink pipeline on $ShmPath"
    $sinkArgs = @(
        "-q",
        "videotestsrc", "is-live=true", "pattern=ball", "!",
        "video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1", "!",
        "queue", "!",
        "shm2sink", "shm-path=$ShmPath", "shm-size=$ShmSizeBytes", "wait-for-connection=true", "consumer-timeout-ms=1000"
    )
    if (-not $DryRun) {
        $sinkProc = Start-Process -FilePath "gst-launch-1.0" -ArgumentList $sinkArgs -RedirectStandardOutput $sinkLogOut -RedirectStandardError $sinkLogErr -PassThru
        Start-Sleep -Seconds 2
        Assert-ProcessAlive -Process $sinkProc -Name "sink"
    }

    $start = Get-Date
    $cycle = 0
    while (((Get-Date) - $start).TotalSeconds -lt $TotalSeconds) {
        $cycle++
        Write-Host "Cycle ${cycle}: attach src for ${AttachSeconds}s"
        $srcArgs = @(
            "-q",
            "shm2src", "shm-path=$ShmPath", "is-live=true", "!",
            "queue", "!", "fakesink", "sync=false"
        )

        if (-not $DryRun) {
            $srcProc = Start-Process -FilePath "gst-launch-1.0" -ArgumentList $srcArgs -RedirectStandardOutput $srcLogOut -RedirectStandardError $srcLogErr -PassThru
            Start-Sleep -Seconds $AttachSeconds
            Assert-ProcessAlive -Process $sinkProc -Name "sink"
            Stop-ProcessIfRunning -Process $srcProc
            $srcProc = $null
        }
        else {
            Start-Sleep -Milliseconds 50
        }

        if (((Get-Date) - $start).TotalSeconds -ge $TotalSeconds) {
            break
        }

        Write-Host "Cycle ${cycle}: detach src for ${DetachSeconds}s"
        if (-not $DryRun) {
            Start-Sleep -Seconds $DetachSeconds
            Assert-ProcessAlive -Process $sinkProc -Name "sink"
        }
        else {
            Start-Sleep -Milliseconds 50
        }
    }

    if (-not $DryRun) {
        Assert-ProcessAlive -Process $sinkProc -Name "sink"
        $sinkTextOut = if (Test-Path $sinkLogOut) { Get-Content $sinkLogOut -Raw } else { "" }
        $sinkTextErr = if (Test-Path $sinkLogErr) { Get-Content $sinkLogErr -Raw } else { "" }
        $sinkText = "$sinkTextOut`n$sinkTextErr"
        if ($sinkText -match "ERROR|CRITICAL|Another shm2src is already connected") {
            throw "FAIL: sink log contains error indicators"
        }
    }

    Write-Host "PASS: sink survived attach/detach cycles for $TotalSeconds seconds on winshm backend"
}
finally {
    Stop-ProcessIfRunning -Process $srcProc
    Stop-ProcessIfRunning -Process $sinkProc
    Write-Host "sink-log-out: $sinkLogOut"
    Write-Host "sink-log-err: $sinkLogErr"
    Write-Host "src-log-out:  $srcLogOut"
    Write-Host "src-log-err:  $srcLogErr"
}
