#
# Package kvr into a Windows x86_64 release archive from prebuilt
# binaries.
#
# Native host: Windows x86_64. A Linux x86_64 host may also package real
# cross-built PE binaries by passing -Platform windows explicitly; in that
# case the Windows runtime smoke test is skipped (logged, not run). All
# other hosts and targets are rejected.
#
# The archive contains exactly, under a single root directory named
# `kvr-v<VERSION>-windows-x86_64`:
#
#   kvr.exe, kvr-gui.exe             the two release binaries (PE x86-64)
#   README.md
#   LICENSE
#   THIRD_PARTY_NOTICES.md
#   assets/fonts/Silkscreen-OFL.txt
#   assets/fonts/VT323-OFL.txt
#   assets/fonts/DotGothic16-OFL.txt
#   assets/pixel-art-logo.png        required, so packaged README links resolve
#
# A `<archive>.sha256` sidecar in sha256sum format is written next to the
# archive, and the checksum is verified before the script exits.
#
# Usage (relative paths are resolved against the repository root):
#
#   Windows host:
#     pwsh scripts/package-release.ps1 v0.1.2 -BinDir target\release -OutputDir dist
#   Linux host (real cross-built PE binaries):
#     pwsh scripts/package-release.ps1 v0.1.2 -Platform windows -BinDir target/x86_64-pc-windows-msvc/release -OutputDir dist
#
# The version (with or without a leading "v") must equal the Cargo package
# version. Python 3 inspects x86-64 PE32+ headers: GUI Subsystem 2, CLI
# Subsystem 3. Both source binaries and the extracted ZIP are checked.
# Native Windows also smoke-tests the extracted CLI. Both binaries must be
# free of Visual C++ runtime DLL imports (dumpbin/objdump inspection).
# The script never overwrites an existing archive or sidecar.

[CmdletBinding()]
param(
    [Parameter(Position = 0, Mandatory = $true,
               HelpMessage = 'Release version with an optional "v" prefix (e.g. v0.1.2). Must equal the Cargo package version.')]
    [string]$TagOrVersion,

    [Parameter(HelpMessage = 'Directory containing the release binaries (default: target/release).')]
    [string]$BinDir = 'target/release',

    [Parameter(HelpMessage = 'Directory to create the archive in (default: dist).')]
    [string]$OutputDir = 'dist',

    [Parameter(HelpMessage = 'Target platform to package (only "windows" is accepted). On a Windows host it is optional (native). On a Linux host it is required and packages cross-built PE binaries.')]
    [ValidateSet('windows')]
    [string]$Platform = ''
)

$ErrorActionPreference = 'Stop'

if ($TagOrVersion -match '^v(\d+\.\d+\.\d+)$') {
    $V = $Matches[1]
}
elseif ($TagOrVersion -match '^(\d+\.\d+\.\d+)$') {
    $V = $Matches[1]
}
else {
    throw "VERSION must be X.Y.Z or vX.Y.Z (got: $TagOrVersion)"
}

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir = Split-Path -Parent $ScriptDir

function Resolve-RepoPath {
    param([string]$Path)
    if ([System.IO.Path]::IsPathRooted($Path)) { return $Path }
    Join-Path $RootDir $Path
}

$BinDirAbs = Resolve-RepoPath $BinDir
$OutputDirAbs = Resolve-RepoPath $OutputDir

# Packaging target is always Windows x86_64 (PE binaries, .zip archive).
# Native host: Windows x86_64. A Linux x86_64 host may package real
# cross-built PE binaries with an explicit -Platform windows.
$hostWindows = ($IsWindows -or $env:OS -eq 'Windows_NT')
$hostLinux = [bool]$IsLinux
if (-not $hostWindows -and -not $hostLinux) {
    throw 'unsupported host platform (expected a Windows x86_64 or Linux x86_64 host)'
}
if ($Platform -and $Platform -ne 'windows') {
    throw "only -Platform windows is accepted (got: $Platform)"
}
if ($hostWindows) {
    if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
        throw "unexpected host architecture: $($env:PROCESSOR_ARCHITECTURE) (expected AMD64)"
    }
}
else {
    if (-not $Platform) {
        throw 'on a Linux host pass -Platform windows to package cross-built PE binaries'
    }
    $unameM = (& uname -m).Trim()
    if ($unameM -ne 'x86_64') {
        throw "unexpected host architecture: $unameM (expected x86_64)"
    }
}
$exe = '.exe'
$ext = 'zip'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo is not on PATH'
}

# Resolve the root package version from the cargo metadata JSON. The root
# package is selected via resolve.root, falling back to the manifest path.
Push-Location $RootDir
try {
    $metaLines = & cargo metadata --locked --no-deps --format-version 1 2>$null
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
}
finally {
    Pop-Location
}
$meta = ($metaLines -join ' ') | ConvertFrom-Json
$rootId = $null
if ($meta.PSObject.Properties.Name -contains 'resolve' -and $meta.resolve) {
    $rootId = $meta.resolve.root
}
$pkg = $meta.packages | Where-Object { $_.id -eq $rootId } | Select-Object -First 1
if (-not $pkg) {
    $rootManifest = [System.IO.Path]::GetFullPath((Join-Path $RootDir 'Cargo.toml'))
    $pkg = $meta.packages | Where-Object {
        [System.IO.Path]::GetFullPath($_.manifest_path) -eq $rootManifest
    } | Select-Object -First 1
}
if (-not $pkg) { throw 'root package not found in cargo metadata' }
$cargoVersion = [string]$pkg.version
if ($cargoVersion -ne $V) {
    throw "version $V does not match the Cargo package version $cargoVersion"
}

$python = Get-Command python3 -ErrorAction SilentlyContinue
if (-not $python) { $python = Get-Command python -ErrorAction SilentlyContinue }
if (-not $python) { throw 'Python 3 is required for portable PE header checks' }
$peChecker = Join-Path $ScriptDir 'check-windows-pe.py'

# Build runners have the redistributable installed, so executing there alone
# cannot detect a release that would fail on a clean Windows machine.
if ($hostWindows) {
    $dumpbin = Get-Command dumpbin -ErrorAction SilentlyContinue
    if ($dumpbin) {
        $dependencyTool = $dumpbin.Source
    }
    else {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
        if (-not (Test-Path -LiteralPath $vswhere)) { throw 'dumpbin not on PATH and vswhere not found' }
        $vsRoot = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($LASTEXITCODE -ne 0 -or -not $vsRoot) { throw 'Visual Studio C++ tools not found' }
        $tools = Get-ChildItem -Path (Join-Path $vsRoot 'VC/Tools/MSVC') -Directory |
            Sort-Object { [version]$_.Name } -Descending
        $dependencyTool = Join-Path $tools[0].FullName 'bin/Hostx64/x64/dumpbin.exe'
        if (-not (Test-Path -LiteralPath $dependencyTool)) { throw "dumpbin not found: $dependencyTool" }
    }
}
else {
    $objdump = Get-Command objdump -ErrorAction SilentlyContinue
    if (-not $objdump) { throw 'objdump is required to inspect Windows runtime dependencies on Linux' }
    $dependencyTool = $objdump.Source
}

function Assert-WindowsBinaries {
    param([string]$Directory)
    & $python.Source $peChecker --cli (Join-Path $Directory 'kvr.exe') --gui (Join-Path $Directory 'kvr-gui.exe')
    if ($LASTEXITCODE -ne 0) { throw "PE subsystem/architecture check failed in $Directory" }
foreach ($bin in @('kvr', 'kvr-gui')) {
    $src = Join-Path $Directory ($bin + $exe)
    if ($hostWindows) {
        $dependencies = & $dependencyTool /nologo /dependents $src 2>&1
    }
    else {
        $dependencies = & $dependencyTool -p $src 2>&1
    }
    if ($LASTEXITCODE -ne 0) { throw "dependency inspection failed for ${src}: $($dependencies -join ' ')" }
    $runtimeDlls = [regex]::Matches(($dependencies -join "`n"), '(?i)\b(?:vcruntime\d+[^ \t\r\n]*|msvcp\d+[^ \t\r\n]*|concrt\d+[^ \t\r\n]*|msvcr\d+[^ \t\r\n]*|ucrtbase[d]?)\.dll\b')
    if ($runtimeDlls.Count -gt 0) {
        throw "$src requires Visual C++ runtime DLLs: $($runtimeDlls.Value -join ', '). Rebuild with the repository's static CRT configuration."
    }
}

# Smoke the CLI on a native Windows host: --help and --version must
# succeed, and --version must report exactly `kvr <VERSION>`.
if ($hostWindows) {
    $cli = Join-Path $Directory ('kvr' + $exe)
    $help = & $cli --help 2>&1
    if ($LASTEXITCODE -ne 0) { throw "kvr --help failed: $($help -join ' ')" }
    $versionOut = & $cli --version 2>&1
    if ($LASTEXITCODE -ne 0) { throw "kvr --version failed: $($versionOut -join ' ')" }
    $joined = ($versionOut -join ' ').Trim()
    if ($joined -ne "kvr $V") {
        throw "kvr --version did not report the exact string 'kvr $V': $joined"
    }
}
else {
    Write-Host 'note: cross-built PE binaries; Windows runtime smoke test (kvr --help/--version) is NOT performed'
}

}

Assert-WindowsBinaries $BinDirAbs
$rootName = "kvr-v$V-windows-x86_64"
$staging = Join-Path ([System.IO.Path]::GetTempPath()) ('kvr-pack-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $staging | Out-Null
$destRoot = Join-Path $staging $rootName
New-Item -ItemType Directory -Path (Join-Path $destRoot 'assets/fonts') | Out-Null

try {
    foreach ($bin in @('kvr', 'kvr-gui')) {
        Copy-Item -LiteralPath (Join-Path $BinDirAbs ($bin + $exe)) -Destination (Join-Path $destRoot ($bin + $exe))
    }

    $required = @(
        'README.md',
        'LICENSE',
        'THIRD_PARTY_NOTICES.md',
        'assets/fonts/Silkscreen-OFL.txt',
        'assets/fonts/VT323-OFL.txt',
        'assets/fonts/DotGothic16-OFL.txt',
        'assets/pixel-art-logo.png'
    )
    foreach ($rel in $required) {
        $src = Join-Path $RootDir $rel
        if (-not (Test-Path -LiteralPath $src -PathType Leaf)) { throw "required file not found: $rel" }
        $dest = Join-Path $destRoot $rel
        New-Item -ItemType Directory -Path (Split-Path -Parent $dest) -Force | Out-Null
        Copy-Item -LiteralPath $src -Destination $dest
    }

    $archiveName = "$rootName.$ext"
    New-Item -ItemType Directory -Path $OutputDirAbs -Force | Out-Null
    $archivePath = Join-Path $OutputDirAbs $archiveName
    $sidecarPath = "$archivePath.sha256"
    if (Test-Path -LiteralPath $archivePath) { throw "output already exists (not overwriting): $archivePath" }
    if (Test-Path -LiteralPath $sidecarPath) { throw "output already exists (not overwriting): $sidecarPath" }

    # Windows PowerShell 5.1's Compress-Archive can emit backslash names.
    # Create entries explicitly so every host writes canonical ZIP paths.
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archiveStream = [System.IO.File]::Open($archivePath, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write)
    try {
        $writer = [System.IO.Compression.ZipArchive]::new($archiveStream, [System.IO.Compression.ZipArchiveMode]::Create, $true)
        try {
            foreach ($file in Get-ChildItem -LiteralPath $destRoot -File -Recurse) {
                $entryName = $file.FullName.Substring($staging.Length + 1).Replace('\', '/')
                [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                    $writer, $file.FullName, $entryName, [System.IO.Compression.CompressionLevel]::Optimal
                ) | Out-Null
            }
        }
        finally { $writer.Dispose() }
    }
    finally { $archiveStream.Dispose() }
    # Enforce the single-root-directory layout.
    $zip = [System.IO.Compression.ZipFile]::OpenRead($archivePath)
    try {
        foreach ($entry in $zip.Entries) {
            if (-not $entry.FullName.StartsWith("$rootName/", [System.StringComparison]::Ordinal)) {
                throw "archive contains entries outside the root directory: $($entry.FullName)"
            }
        }
    }
    finally { $zip.Dispose() }
    $extracted = Join-Path $staging 'extracted'
    Expand-Archive -LiteralPath $archivePath -DestinationPath $extracted
    Assert-WindowsBinaries (Join-Path $extracted $rootName)

    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $archivePath).Hash.ToLowerInvariant()
    if ($hash -notmatch '^[0-9a-f]{64}$') { throw "unexpected SHA-256 output: $hash" }
    $sidecarStream = [System.IO.File]::Open($sidecarPath, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write)
    try {
        $sidecarBytes = [System.Text.Encoding]::UTF8.GetBytes("$hash  $archiveName`n")
        $sidecarStream.Write($sidecarBytes, 0, $sidecarBytes.Length)
    }
    finally { $sidecarStream.Dispose() }

    $parts = ([System.IO.File]::ReadAllText($sidecarPath)).Trim() -split '\s+'
    if ($parts.Count -ne 2 -or $parts[0] -ne $hash -or $parts[1] -ne $archiveName) {
        throw "checksum sidecar verification failed for $archiveName"
    }
    $recheck = (Get-FileHash -Algorithm SHA256 -LiteralPath $archivePath).Hash.ToLowerInvariant()
    if ($recheck -ne $hash) { throw "checksum verification failed for $archiveName" }

    Write-Host "wrote $archivePath"
    Write-Host "wrote $sidecarPath"
}
finally {
    Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue
}
