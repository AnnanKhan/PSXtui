<#
.SYNOPSIS
    psxtui installer for Windows.

.DESCRIPTION
    By default this downloads the prebuilt psxtui.exe from GitHub Releases, so
    nothing has to be compiled and no Rust toolchain is needed. -FromSource
    builds it instead, which needs Rust 1.88+ and the Visual Studio C++ build
    tools (for the bundled SQLite).

    Everything it does is printed as it goes, so nothing happens that you could
    not have typed yourself.

.PARAMETER FromSource
    Build with cargo instead of downloading a release binary.

.PARAMETER NoModifyPath
    Never touch the user PATH.

.PARAMETER Uninstall
    Remove psxtui and its PATH entry. The cache and watchlist are kept.

.EXAMPLE
    .\install.ps1
    .\install.ps1 -FromSource
    .\install.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [switch]$FromSource,
    [switch]$NoModifyPath,
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

$Msrv       = [version]'1.88'
$Repo       = 'AnnanKhan/PSXtui'
$Target     = 'x86_64-pc-windows-msvc'
# Not %APPDATA%: this is a program, not roaming state, and a binary that
# follows the user onto another machine is a binary that may not run there.
$InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\psxtui'
$Exe        = Join-Path $InstallDir 'psxtui.exe'

function Say  ($m) { Write-Host ':: ' -ForegroundColor Blue   -NoNewline; Write-Host $m }
function Warn ($m) { Write-Host '!! ' -ForegroundColor Yellow -NoNewline; Write-Host $m }
function Die  ($m) { Write-Host 'xx ' -ForegroundColor Red    -NoNewline; Write-Host $m; exit 1 }
function Have ($c) { $null -ne (Get-Command $c -ErrorAction SilentlyContinue) }

# --- PATH -----------------------------------------------------------------

function Add-ToUserPath($dir) {
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($current -split ';' -contains $dir) { return }
    $updated = if ([string]::IsNullOrEmpty($current)) { $dir } else { "$current;$dir" }
    [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
    # The running shell keeps its own copy, so make this session work too.
    $env:Path = "$env:Path;$dir"
    Say "added $dir to your user PATH — open a new terminal for it to stick"
}

function Remove-FromUserPath($dir) {
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ([string]::IsNullOrEmpty($current)) { return }
    $kept = ($current -split ';' | Where-Object { $_ -and $_ -ne $dir }) -join ';'
    if ($kept -ne $current) {
        [Environment]::SetEnvironmentVariable('Path', $kept, 'User')
        Say "removed $dir from your user PATH"
    }
}

# --- uninstall ------------------------------------------------------------

if ($Uninstall) {
    $removed = $false
    if (Test-Path $Exe) {
        Remove-Item $Exe -Force
        Say "removed $Exe"
        $removed = $true
    }
    $cargoExe = Join-Path $env:USERPROFILE '.cargo\bin\psxtui.exe'
    if (Test-Path $cargoExe) {
        if (Have cargo) { cargo uninstall psxtui | Out-Null } else { Remove-Item $cargoExe -Force }
        Say "removed $cargoExe"
        $removed = $true
    }
    if (-not $NoModifyPath) { Remove-FromUserPath $InstallDir }
    if (-not $removed) { Warn 'no psxtui installation found' }
    Say "cache and watchlist left alone: $env:APPDATA\psxtui\data\psx.db"
    exit 0
}

# --- 1. prebuilt binary ---------------------------------------------------

function Install-Prebuilt {
    Say "looking for a released build of psxtui ($Target)"
    try {
        # No token needed for public releases; the UA keeps GitHub happy.
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
            -Headers @{ 'User-Agent' = 'psxtui-installer' }
    } catch {
        Warn "could not reach the GitHub releases API: $($_.Exception.Message)"
        return $false
    }

    $asset = $release.assets | Where-Object { $_.name -like "*$Target.zip" } | Select-Object -First 1
    if (-not $asset) {
        Warn "release $($release.tag_name) has no $Target asset"
        return $false
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("psxtui-" + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp -Force | Out-Null
    try {
        $zip = Join-Path $tmp $asset.name
        Say "downloading $($asset.name) ($([math]::Round($asset.size / 1MB, 1)) MB)"
        Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zip -UseBasicParsing
        Expand-Archive -Path $zip -DestinationPath $tmp -Force

        $built = Get-ChildItem -Path $tmp -Filter 'psxtui.exe' -Recurse | Select-Object -First 1
        if (-not $built) { Warn 'the archive contained no psxtui.exe'; return $false }

        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        # A running psxtui holds its own exe open; say so rather than failing
        # with a bare access-denied.
        try {
            Copy-Item $built.FullName $Exe -Force
        } catch {
            Die "could not write $Exe — close any running psxtui and try again"
        }
        Say "installed $Exe ($($release.tag_name))"
        return $true
    } finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# --- 2. building from source ----------------------------------------------

function Install-FromSource {
    # The MSRV is not cosmetic: the code uses let-chains, which landed in 1.88.
    $ok = $false
    if (Have rustc) {
        $v = [version]((rustc --version) -split ' ')[1]
        $ok = $v -ge $Msrv
        if ($ok) { Say "using rustc $v" } else { Warn "rustc $v is older than $Msrv" }
    }

    if (-not $ok) {
        if (Have rustup) {
            Say 'updating the stable toolchain'
            rustup update stable
            rustup default stable
        } elseif (Have winget) {
            Say 'installing Rust with winget (Rustlang.Rustup)'
            winget install --id Rustlang.Rustup --silent --accept-source-agreements --accept-package-agreements
            $env:Path = "$env:Path;$env:USERPROFILE\.cargo\bin"
        } else {
            Die "install Rust $Msrv or newer from https://rustup.rs and re-run this script"
        }
    }

    if (-not (Have cargo)) { Die 'cargo is not on PATH — open a new terminal and re-run' }

    # rustup installs no C compiler, and the bundled SQLite needs one.
    if (-not (Have cl) -and -not (Have link)) {
        Warn 'no MSVC C++ compiler found — the bundled SQLite cannot build without it'
        Warn 'install "Desktop development with C++" from the Visual Studio Build Tools:'
        Warn '  winget install Microsoft.VisualStudio.2022.BuildTools'
        Warn 'then run this from a "Developer PowerShell for VS" window'
    }

    $src = $PSScriptRoot
    if (-not (Test-Path (Join-Path $src 'Cargo.toml'))) {
        if (-not (Have git)) { Die 'no Cargo.toml beside this script and no git to clone with' }
        $src = Join-Path ([IO.Path]::GetTempPath()) ("psxtui-src-" + [guid]::NewGuid())
        Say "cloning https://github.com/$Repo into $src"
        git clone --depth 1 "https://github.com/$Repo.git" $src
    }

    Say 'building (a first release build takes a few minutes)'
    cargo install --path $src --force
    if ($LASTEXITCODE -ne 0) { Die 'the build failed — see the cargo output above' }

    $script:Exe = Join-Path $env:USERPROFILE '.cargo\bin\psxtui.exe'
    $script:InstallDir = Split-Path $script:Exe
    Say "installed $script:Exe"
}

# --- run ------------------------------------------------------------------

if ($FromSource) {
    Install-FromSource
} elseif (-not (Install-Prebuilt)) {
    Warn 'falling back to building from source'
    Install-FromSource
}

if (-not $NoModifyPath) { Add-ToUserPath $InstallDir }

Say 'installed:'
& $Exe --version

Write-Host @"

  Run it with:   psxtui
  Keys:          ? inside the app
  Data lives in: $env:APPDATA\psxtui\data\psx.db

Use Windows Terminal — it is the one that does truecolour, mouse reporting and
the box-drawing glyphs the UI is built from. If charts render as empty boxes,
your font has no braille: install a Nerd Font, or set PSXTUI_MARKER=block.

The first launch fetches the market board and backfills ~120 days of history in
the background — it is usable immediately and gets richer as that lands.
"@
