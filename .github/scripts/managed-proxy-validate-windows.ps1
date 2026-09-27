$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

# Check the actual process token before invoking any daemon or test executable.
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $env:MANAGED_PROXY_EXPECTED_SID) {
    throw "Expected standard-user SID was not delivered to child; actual SID $($identity.User.Value)"
}
if ($identity.User.Value -ne $env:MANAGED_PROXY_EXPECTED_SID) {
    throw "Child SID $($identity.User.Value) differs from expected SID $env:MANAGED_PROXY_EXPECTED_SID"
}
$groups = & whoami.exe /groups /fo csv /nh | ConvertFrom-Csv -Header Name, Type, SID, Attributes
if ($LASTEXITCODE -ne 0 -or @($groups | Where-Object SID -eq 'S-1-5-32-544').Count -ne 0) {
    throw 'Child token contains the Administrators group'
}
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class ManagedProxyToken {
    [DllImport("advapi32.dll", SetLastError = true)]
    public static extern bool GetTokenInformation(IntPtr token, int informationClass,
        out int information, int informationLength, out int returnLength);
}
'@
$elevation = 1
$length = 0
if (-not [ManagedProxyToken]::GetTokenInformation($identity.Token, 20, [ref]$elevation, 4, [ref]$length) -or $elevation -ne 0) {
    throw 'Child token elevation verification failed'
}
Write-Host "Verified standard-user token SID $($identity.User.Value), TokenElevation=0"
# The launcher supplies a deliberately fresh environment. Resolve the loaded
# profile's Known Folders under this verified token before launching tools.
$accountProfile = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
$applicationData = [Environment]::GetFolderPath([Environment+SpecialFolder]::ApplicationData, [Environment+SpecialFolderOption]::Create)
$localApplicationData = [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData, [Environment+SpecialFolderOption]::Create)
foreach ($path in @($accountProfile, $applicationData, $localApplicationData)) {
    if (-not $path -or -not (Test-Path $path -PathType Container)) {
        throw 'Child could not resolve its loaded account profile'
    }
}
$env:USERPROFILE = $accountProfile
$env:APPDATA = $applicationData
$env:LOCALAPPDATA = $localApplicationData

$binDir = [IO.Path]::GetFullPath((Join-Path $env:CARGO_HOME '..\bin'))
foreach ($tool in @('cargo', 'rustc', 'cargo-nextest', 'just', 'dotslash', 'uv')) {
    $resolved = (Get-Command "$tool.exe" -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    if (-not $resolved.Equals((Join-Path $binDir "$tool.exe"), [StringComparison]::OrdinalIgnoreCase)) {
        throw "Tool $tool did not resolve from the task-scoped tool directory: $resolved"
    }
}
foreach ($command in @(@('rustc.exe', '--version'), @('cargo.exe', '--version'), @('just.exe', '--version'), @('cargo.exe', 'nextest', '--version'))) {
    $executable = $command[0]
    $arguments = $command[1..($command.Count - 1)]
    & $executable @arguments
    if ($LASTEXITCODE -ne 0) { throw "Tool preflight failed: $($command[0])" }
}
if ((& rustc.exe --version) -notmatch [regex]::Escape($env:RUSTUP_TOOLCHAIN.Split('-')[0])) {
    throw 'Active rustc does not match the pinned toolchain'
}

Set-Location (Join-Path $env:GITHUB_WORKSPACE 'codex-rs')
& git.exe config --global --add safe.directory $env:GITHUB_WORKSPACE
if ($LASTEXITCODE -ne 0) { throw 'Could not configure Git safe.directory for the child profile' }
$failed = [Collections.Generic.List[string]]::new()
function Invoke-Check {
    param([string]$Name, [string]$Command, [string[]]$Arguments)
    Write-Host "Running $Name"
    try {
        & $Command @Arguments
        if ($LASTEXITCODE -ne 0) { throw "exit code $LASTEXITCODE" }
    } catch {
        Write-Host "$Name failed: $($_.Exception.Message)"
        $failed.Add($Name)
    }
}

Invoke-Check 'CLI suite' 'just.exe' @('test', '-p', 'codex-cli')
Invoke-Check 'Lifecycle suite' 'just.exe' @('test', '-p', 'codex-app-server-daemon')
Invoke-Check 'Proxy transport suite' 'just.exe' @('test', '-p', 'codex-stdio-to-uds')
if ($env:MANAGED_PROXY_V8_READY -eq 'True') {
    Invoke-Check 'App-server helper build' 'cargo.exe' @('build', '-p', 'codex-code-mode-host', '-p', 'codex-rmcp-client', '--bins')
} else {
    $failed.Add('V8 setup')
}
Invoke-Check 'App-server suite' 'just.exe' @('test', '-p', 'codex-app-server')

# Keep repository fix/fmt as the final commands; do not test after them.
Invoke-Check 'CLI lint fixes' 'just.exe' @('fix', '-p', 'codex-cli')
Invoke-Check 'Repository formatting' 'just.exe' @('fmt')
Invoke-Check 'Clean tracked tree' 'git.exe' @('diff', '--exit-code')
$untracked = & git.exe ls-files --others --exclude-standard
if ($LASTEXITCODE -ne 0 -or $untracked) { $failed.Add('Clean untracked tree') }
if ($failed.Count) {
    Write-Host "Failed checks: $($failed -join ', ')"
    exit 1
}
Write-Host 'All standard-user checks passed'
