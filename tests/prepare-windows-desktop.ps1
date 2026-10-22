# Test-only preparation for GitHub's disposable Windows ARM64 image.
# A Windows web-host overlay can cover even topmost fixture windows. Cancel it;
# do not accept setup choices, change security settings or touch user desktops.
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or
    $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or
    $env:RUNNER_OS -ne 'Windows' -or $env:RUNNER_ARCH -ne 'ARM64') {
    exit 0
}
$session = (Get-Process -Id $PID).SessionId
$expected = [IO.Path]::Combine([Environment]::SystemDirectory, 'WWAHost.exe')
foreach ($process in @(Get-Process -Name WWAHost -ErrorAction SilentlyContinue)) {
    try {
        if ($process.SessionId -ne $session -or
            -not [string]::Equals($process.Path, $expected, [StringComparison]::OrdinalIgnoreCase)) {
            continue
        }
        if ($process.HasExited) { continue }
        $closed = $process.CloseMainWindow()
        if (-not $closed -or -not $process.WaitForExit(2000)) {
            # This VM contains only the checked-out test project, with no user apps.
            if (-not $process.HasExited) { $process.Kill() }
            if (-not $process.WaitForExit(3000)) { throw 'Windows web-host overlay did not close' }
        }
        Write-Host 'Closed the Windows web-host overlay in disposable ARM64 CI.'
    } finally {
        $process.Dispose()
    }
}
