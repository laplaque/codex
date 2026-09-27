# Run the managed-proxy validation in a real local standard-user logon session.
$ErrorActionPreference = 'Stop'

function Invoke-Icacls {
    & icacls.exe @args | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "icacls failed with exit code $LASTEXITCODE" }
}

$name = 'mpv' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
$taskRoot = Join-Path ($env:CI_BUILD_ROOT + '\') "managed-proxy-$env:GITHUB_RUN_ID-$env:GITHUB_RUN_ATTEMPT"
$account = $null
try {
    $bytes = [System.Security.Cryptography.RandomNumberGenerator]::GetBytes(32)
    $password = ConvertTo-SecureString ('Aa1!' + [Convert]::ToBase64String($bytes)) -AsPlainText -Force
    [Array]::Clear($bytes, 0, $bytes.Length)
    $account = New-LocalUser -Name $name -Password $password -AccountNeverExpires -ErrorAction Stop
    if (-not (Get-LocalGroupMember -Group 'Users' | Where-Object { $_.SID.Value -eq $account.SID.Value })) {
        Add-LocalGroupMember -Group 'Users' -Member $account -ErrorAction Stop
    }
    $credential = [pscredential]::new("$env:COMPUTERNAME\$name", $password)
    $sid = $account.SID.Value
    $runnerSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value

    New-Item -ItemType Directory -Path $taskRoot -ErrorAction Stop | Out-Null
    Invoke-Icacls $taskRoot '/inheritance:r'
    Invoke-Icacls $taskRoot '/grant:r' ("*${runnerSid}:(OI)(CI)F") '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' ("*${sid}:(OI)(CI)M")

    $cargoHome = Join-Path $taskRoot 'cargo'
    $rustupHome = Join-Path $taskRoot 'rustup'
    $binDir = Join-Path $taskRoot 'bin'
    foreach ($path in @($cargoHome, $rustupHome, $binDir, (Join-Path $taskRoot 'tmp'), (Join-Path $taskRoot 'target'), (Join-Path $taskRoot 'codex-home'))) {
        New-Item -ItemType Directory -Path $path -ErrorAction Stop | Out-Null
    }

    # The installed toolchain and user-local action tools belong to the runner's
    # profile. Copy only the needed binaries/toolchain into the task's ACL scope.
    $pin = (Select-String -Path (Join-Path $env:GITHUB_WORKSPACE 'codex-rs/rust-toolchain.toml') -Pattern '^\s*channel\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
    $sourceRustup = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
    $toolchain = Get-ChildItem (Join-Path $sourceRustup 'toolchains') -Directory |
        Where-Object { $_.Name -eq $pin -or $_.Name.StartsWith("$pin-") } |
        Select-Object -First 1
    if (-not $toolchain) { throw "Pinned Rust toolchain $pin was not installed" }
    $toolchainsDir = Join-Path $rustupHome 'toolchains'
    New-Item -ItemType Directory -Path $toolchainsDir -ErrorAction Stop | Out-Null
    Copy-Item $toolchain.FullName -Destination (Join-Path $toolchainsDir $toolchain.Name) -Recurse -ErrorAction Stop
    foreach ($tool in @('cargo', 'rustc', 'rustup', 'cargo-nextest', 'just', 'dotslash', 'uv')) {
        $command = Get-Command "$tool.exe" -CommandType Application -ErrorAction Stop
        Copy-Item $command.Source (Join-Path $binDir "$tool.exe") -ErrorAction Stop
    }

    $v8Ready = $env:MANAGED_PROXY_V8_READY -eq 'true'
    $childEnvironment = @{
        CARGO_HOME = $cargoHome
        RUSTUP_HOME = $rustupHome
        RUSTUP_TOOLCHAIN = $toolchain.Name
        CARGO_TARGET_DIR = (Join-Path $taskRoot 'target')
        CARGO_NET_GIT_FETCH_WITH_CLI = 'true'
        CODEX_HOME = (Join-Path $taskRoot 'codex-home')
        CODEX_REPO_ROOT = $env:GITHUB_WORKSPACE
        TEMP = (Join-Path $taskRoot 'tmp')
        TMP = (Join-Path $taskRoot 'tmp')
        UV_CACHE_DIR = (Join-Path $taskRoot 'uv-cache')
        XDG_CACHE_HOME = (Join-Path $taskRoot 'cache')
        BAZEL_OUTPUT_BASE = (Join-Path $taskRoot 'bazel-output')
        BAZEL_OUTPUT_USER_ROOT = (Join-Path $taskRoot 'bazel-user')
        BAZEL_REPOSITORY_CACHE = (Join-Path $taskRoot 'bazel-repository-cache')
        BAZEL_REPO_CONTENTS_CACHE = (Join-Path $taskRoot 'bazel-repo-contents-cache')
        GITHUB_WORKSPACE = $env:GITHUB_WORKSPACE
        GITHUB_ACTIONS = 'true'
        CI = 'true'
        MANAGED_PROXY_EXPECTED_SID = $sid
        MANAGED_PROXY_V8_READY = [string]$v8Ready
    }
    if ($v8Ready) {
        foreach ($key in @('RUSTY_V8_ARCHIVE', 'RUSTY_V8_SRC_BINDING_PATH')) {
            $source = [Environment]::GetEnvironmentVariable($key)
            if (-not $source -or -not (Test-Path $source -PathType Leaf)) { throw "Missing verified V8 input: $key" }
            $destination = Join-Path $taskRoot (Split-Path $source -Leaf)
            Copy-Item $source $destination -ErrorAction Stop
            $childEnvironment[$key] = $destination
        }
    }
    foreach ($key in @('LIB', 'INCLUDE', 'LIBPATH', 'VCToolsInstallDir', 'WindowsSdkDir')) {
        $value = [Environment]::GetEnvironmentVariable($key)
        if ($value) { $childEnvironment[$key] = $value }
    }
    $machinePath = $env:PATH -split ';' | Where-Object {
        $_ -and -not $_.StartsWith($env:USERPROFILE, [StringComparison]::OrdinalIgnoreCase) -and
        -not $_.StartsWith($env:RUNNER_TEMP, [StringComparison]::OrdinalIgnoreCase)
    }
    $childEnvironment['PATH'] = (@($binDir) + $machinePath) -join ';'

    Invoke-Icacls $env:GITHUB_WORKSPACE '/grant:r' ("*${sid}:(OI)(CI)M") '/T'
    $stdout = Join-Path $taskRoot 'stdout.log'
    $stderr = Join-Path $taskRoot 'stderr.log'
    $child = Start-Process -FilePath (Get-Command pwsh.exe).Source `
        -ArgumentList @('-NoLogo', '-NoProfile', '-File', (Join-Path $env:GITHUB_WORKSPACE '.github/scripts/managed-proxy-validate-windows.ps1')) `
        -Credential $credential -LoadUserProfile -UseNewEnvironment -Environment $childEnvironment `
        -WorkingDirectory (Join-Path $env:GITHUB_WORKSPACE 'codex-rs') `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru -ErrorAction Stop
    $child.WaitForExit()
    Get-Content $stdout | Write-Host
    Get-Content $stderr | Write-Host
    if ($child.ExitCode -ne 0) { throw "Standard-user validation failed with exit code $($child.ExitCode)" }
} finally {
    if ($account) { Remove-LocalUser -Name $name -ErrorAction Stop }
}
