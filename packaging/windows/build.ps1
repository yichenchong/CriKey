<#
.SYNOPSIS
    Stage the CriKey release binary and build a per-user MSI, an MSIX, or both.

    A Windows build of CriKey is not just crikey.exe. sdk_root() in
    crikey-python-host looks for a 'modern-sdk' directory beside the running
    executable, shim_root() in crikey-legacy-compat looks for 'legacy-shim',
    and the WASM/C-ABI providers look for crikey-wasm-host.exe and
    crikey-cabi-host.exe beside that same executable. Ship the bare executable,
    both supervised runtime hosts, modern Python plugins and every legacy
    Keypirinha package or one of those runtimes stops loading only when invoked.

    It drives, it does not reimplement. The MSI is produced by the WiX v4
    toolset from packaging\windows\crikey.wxs; the MSIX is produced by
    makeappx.exe from packaging\windows\AppxManifest.xml. When the required
    toolchain is not installed the script stops with a named error saying which
    tool was missing and how to obtain it, rather than half-producing an
    artefact.

    Signing is delegated to packaging\windows\sign.ps1, which takes its
    certificate from the environment. No certificate, thumbprint or password is
    ever read from this repository, and nothing here disables or skips
    signature verification. The four staged executables are signed before they
    are packaged and the package is signed after it is built: an MSI signature
    covers the .msi file only, so an unsigned payload inside a signed MSI still
    meets SmartScreen on the first launch.

    THIS SCRIPT REQUIRES A REAL WINDOWS HOST. WiX, makeappx.exe and
    signtool.exe are Windows binaries; there is no cross-platform substitute
    and none is faked.

.PARAMETER Binary
    Path to the release crikey.exe, the command-line entry point. Defaults to
    target\release\crikey.exe under the repository root.

.PARAMETER LauncherBinary
    Path to the release crikey-launcher.exe, the GUI entry point the Start
    Menu shortcut and the MSIX tile start. It takes no arguments. Both binaries
    come out of one `cargo build --release --package crikey-cli`.

.PARAMETER WasmHostBinary
    Path to the release crikey-wasm-host.exe worker. Defaults to
    target\release\crikey-wasm-host.exe and is installed beside the launcher.

.PARAMETER CAbiHostBinary
    Path to the release crikey-cabi-host.exe worker. Defaults to
    target\release\crikey-cabi-host.exe and is installed beside the launcher.

.PARAMETER Version
    Three-part product version. Defaults to the workspace version in
    Cargo.toml. The MSIX manifest needs four parts, so '.0' is appended for it.

.PARAMETER OutputDirectory
    Where the staging tree and the artefacts are written. Defaults to
    target\packaging\windows under the repository root.

.PARAMETER Format
    'msi', 'msix' or 'both'. Defaults to 'msi'.

.PARAMETER PythonRuntimeArchive
    A python-build-standalone archive to stage as python-runtime\ beside the
    executable, via packaging\stage-python-runtime.sh. That stager is a POSIX
    shell script, so this option needs bash on PATH (Git for Windows provides
    it). Optional: without it no interpreter is bundled and CriKey falls back
    to whatever python discovery finds on the target machine. The installer
    does not claim to provide an interpreter it did not stage.

.PARAMETER MsixAssets
    Directory holding StoreLogo.png, Square150x150Logo.png and
    Square44x44Logo.png. Required for the MSIX route; no artwork is checked
    into this repository.

.PARAMETER MsixPublisher
    The Subject of the signing certificate, for example
    'CN=Example Ltd, O=Example Ltd, C=GB'. It must match the certificate
    byte-for-byte or the package will not install. Derived from the signing
    certificate when -CertificateThumbprint is given.

.PARAMETER CertificateThumbprint
    Thumbprint of a code-signing certificate in the current user's certificate
    store. Defaults to $env:CRIKEY_WINDOWS_CERT_THUMBPRINT.

.PARAMETER CertificatePath
    Path to a PFX file, used with the password in
    $env:CRIKEY_WINDOWS_CERT_PASSWORD. Defaults to
    $env:CRIKEY_WINDOWS_CERT_PATH.

.PARAMETER TimestampUrl
    RFC 3161 timestamp authority. A signature without a timestamp stops
    validating the day the certificate expires.

.PARAMETER Unsigned
    Build without signing. For local testing only: an unsigned MSIX cannot be
    installed at all, and an unsigned MSI raises an unknown-publisher warning.

.PARAMETER Validate
    Run `wix msi validate` on the built MSI. Off by default because `wix build`
    does not validate either -- only an MSBuild .wixproj does -- and because
    validation runs the stock ICE custom actions through Windows Installer,
    which the non-interactive service accounts on hosted CI machines cannot do.
    On a developer machine it works, and the three ICEs it suppresses are
    argued per ICE in crikey.wxs. Ignored for the MSIX route.

.EXAMPLE
    .\packaging\windows\build.ps1 -Format msi

.EXAMPLE
    $env:CRIKEY_WINDOWS_CERT_THUMBPRINT = '...'
    .\packaging\windows\build.ps1 -Format both -MsixAssets .\artwork
#>
[CmdletBinding()]
param(
    [string] $Binary,
    [string] $LauncherBinary,
    [string] $WasmHostBinary,
    [string] $CAbiHostBinary,
    [string] $Version,
    [string] $OutputDirectory,
    [ValidateSet('msi', 'msix', 'both')]
    [string] $Format = 'msi',
    [string] $PythonRuntimeArchive,
    [string] $MsixAssets,
    [string] $MsixPublisher,
    [string] $CertificateThumbprint = $env:CRIKEY_WINDOWS_CERT_THUMBPRINT,
    [string] $CertificatePath = $env:CRIKEY_WINDOWS_CERT_PATH,
    [string] $TimestampUrl = 'http://timestamp.digicert.com',
    [switch] $Unsigned,
    [switch] $Validate
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$program = 'crikey-packaging(windows/build)'
$scriptDir = Split-Path -Parent $PSCommandPath
$rootDir = Resolve-Path (Join-Path $scriptDir '..\..')

function Stop-WithError {
    param([string] $Message)
    throw "${program}: error: $Message"
}

function Write-Note {
    param([string] $Message)
    Write-Host "${program}: $Message"
}

# A missing tool is named, with what provides it. A packaging run that dies
# three commands later on a path that does not exist is not a diagnosis.
function Resolve-RequiredTool {
    param(
        [string] $Name,
        [string] $ProvidedBy,
        [string[]] $SearchPaths = @()
    )

    $found = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue
    if ($found) {
        return $found.Source
    }

    foreach ($candidate in $SearchPaths) {
        if (Test-Path -LiteralPath $candidate) {
            return $candidate
        }
    }

    Stop-WithError "required tool '$Name' was not found (provided by $ProvidedBy). This script only runs on Windows."
}

# The Windows SDK does not put makeappx.exe or signtool.exe on PATH. They live
# under `Windows Kits\10\bin\<sdk version>\<arch>\`, and a machine that ever
# carried an older SDK also has a legacy unversioned `bin\<arch>\` beside the
# versioned ones. Sorting the full paths as text puts `\bin\x64\` above
# `\bin\10.0.26100.0\x64\`, because 'x' sorts above '1' -- so a plain
# descending sort picks the oldest tool on exactly the machines that have one.
# An SDK-8.1-era makeappx does not know the uap5 namespace AppxManifest.xml
# uses and rejects it with a schema error that names the wrong cause. The
# version segment is therefore parsed and compared as a version, and the
# unversioned directory is only a fallback.
function Find-WindowsSdkTool {
    param([string] $Name)

    $roots = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\bin",
        "$env:ProgramFiles\Windows Kits\10\bin"
    )

    $candidates = @()
    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root)) {
            continue
        }
        # x64 only: among versioned candidates a text sort otherwise prefers
        # x86, and this package is built and signed for x64 hosts.
        $candidates += Get-ChildItem -Path $root -Filter $Name -Recurse -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\x64\\' }
    }

    if ($candidates.Count -eq 0) {
        return $null
    }

    $versioned = $candidates |
        Where-Object { $_.FullName -match '\\bin\\(\d+(?:\.\d+){3})\\x64\\' } |
        Sort-Object -Property @{ Expression = { [version]([regex]::Match($_.FullName, '\\bin\\(\d+(?:\.\d+){3})\\x64\\').Groups[1].Value) } } -Descending
    if ($versioned) {
        return (@($versioned)[0]).FullName
    }

    return (@($candidates)[0]).FullName
}

function Copy-PythonPayload {
    param(
        [string] $Source,
        [string] $Destination
    )

    if (-not (Test-Path -LiteralPath $Source)) {
        Stop-WithError "payload directory '$Source' is missing"
    }

    Copy-Item -LiteralPath $Source -Destination $Destination -Recurse -Force

    # __pycache__ holds bytecode compiled by whatever interpreter last ran in
    # the source tree: stale by construction on a user's machine, and pure
    # churn inside a signed package.
    Get-ChildItem -Path $Destination -Filter '__pycache__' -Recurse -Directory -ErrorAction SilentlyContinue |
        ForEach-Object { Remove-Item -LiteralPath $_.FullName -Recurse -Force }
}

if (-not $Binary) {
    $Binary = Join-Path $rootDir 'target\release\crikey.exe'
}
if (-not (Test-Path -LiteralPath $Binary)) {
    Stop-WithError "binary '$Binary' does not exist; build it first with: cargo build --release --package crikey-cli"
}

if (-not $LauncherBinary) {
    $LauncherBinary = Join-Path $rootDir 'target\release\crikey-launcher.exe'
}
if (-not (Test-Path -LiteralPath $LauncherBinary)) {
    # Not optional: the MSI shortcut and the MSIX Application element both name
    # this executable, so an artefact staged without it installs and then fails
    # to start from the only entry points a user has.
    Stop-WithError "GUI binary '$LauncherBinary' does not exist; build it first with: cargo build --release --package crikey-cli"
}
if (-not $WasmHostBinary) {
    $WasmHostBinary = Join-Path $rootDir 'target\release\crikey-wasm-host.exe'
}
if (-not (Test-Path -LiteralPath $WasmHostBinary)) {
    Stop-WithError "WASM host '$WasmHostBinary' does not exist; build it with: cargo build --release --package crikey-wasm-host"
}

if (-not $CAbiHostBinary) {
    $CAbiHostBinary = Join-Path $rootDir 'target\release\crikey-cabi-host.exe'
}
if (-not (Test-Path -LiteralPath $CAbiHostBinary)) {
    Stop-WithError "C-ABI host '$CAbiHostBinary' does not exist; build it with: cargo build --release --package crikey-cabi-host"
}

if (-not $Version) {
    # The workspace version every crate inherits, read out of the
    # [workspace.package] table rather than by running cargo, so packaging does
    # not require a Rust toolchain on the signing host.
    $versionLine = Select-String -Path (Join-Path $rootDir 'Cargo.toml') -Pattern '^version = "(.*)"$' |
        Select-Object -First 1
    if (-not $versionLine) {
        Stop-WithError "could not read the workspace version from Cargo.toml; pass -Version"
    }
    $Version = $versionLine.Matches[0].Groups[1].Value
}

if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $rootDir 'target\packaging\windows'
}
$stage = Join-Path $OutputDirectory 'stage'

# A stale staging tree merged with a new one is how a file nobody meant to ship
# gets shipped. Staging always starts from nothing.
if (Test-Path -LiteralPath $stage) {
    Remove-Item -LiteralPath $stage -Recurse -Force
}
New-Item -ItemType Directory -Path $stage -Force | Out-Null

Write-Note "staging into $stage (version $Version)"

Copy-Item -LiteralPath $Binary -Destination (Join-Path $stage 'crikey.exe') -Force
Copy-Item -LiteralPath $LauncherBinary -Destination (Join-Path $stage 'crikey-launcher.exe') -Force
Copy-Item -LiteralPath $WasmHostBinary -Destination (Join-Path $stage 'crikey-wasm-host.exe') -Force
Copy-Item -LiteralPath $CAbiHostBinary -Destination (Join-Path $stage 'crikey-cabi-host.exe') -Force

# Spec 14.13: the licence and the attribution notice travel with the artefact.
# LICENSE is renamed on the way in because an extensionless file on Windows
# opens the "how do you want to open this" dialog instead of the licence.
Copy-Item -LiteralPath (Join-Path $rootDir 'LICENSE') -Destination (Join-Path $stage 'LICENSE.txt') -Force
Copy-Item -LiteralPath (Join-Path $rootDir 'NOTICE.md') -Destination (Join-Path $stage 'NOTICE.md') -Force

Copy-PythonPayload -Source (Join-Path $rootDir 'sdk\python') -Destination (Join-Path $stage 'modern-sdk')
if (-not (Test-Path -LiteralPath (Join-Path $stage 'modern-sdk\_crikey_modern_worker.py'))) {
    Stop-WithError "sdk\python has no _crikey_modern_worker.py; sdk_root() would reject the staged directory"
}

Copy-PythonPayload -Source (Join-Path $rootDir 'crates\crikey-legacy-compat\python') -Destination (Join-Path $stage 'legacy-shim')
if (-not (Test-Path -LiteralPath (Join-Path $stage 'legacy-shim\_crikey_legacy_worker.py'))) {
    Stop-WithError "the legacy shim has no _crikey_legacy_worker.py; shim_root() would reject the staged directory"
}

$havePythonRuntime = $false
if ($PythonRuntimeArchive) {
    if (-not (Test-Path -LiteralPath $PythonRuntimeArchive)) {
        Stop-WithError "python runtime archive '$PythonRuntimeArchive' does not exist"
    }
    $bash = Resolve-RequiredTool -Name 'bash.exe' -ProvidedBy 'Git for Windows'
    $stager = Join-Path $rootDir 'packaging\stage-python-runtime.sh'
    if (-not (Test-Path -LiteralPath $stager)) {
        Stop-WithError "-PythonRuntimeArchive was given but $stager is missing"
    }
    Write-Note 'staging the bundled Python runtime'
    & $bash $stager --dest $stage --archive $PythonRuntimeArchive
    if ($LASTEXITCODE -ne 0) {
        Stop-WithError "the Python runtime stager failed with exit code $LASTEXITCODE"
    }
    if (-not (Test-Path -LiteralPath (Join-Path $stage 'python-runtime\python.exe'))) {
        Stop-WithError "the stager did not produce python-runtime\python.exe"
    }
    $havePythonRuntime = $true
}
else {
    Write-Note 'no -PythonRuntimeArchive: no interpreter is bundled, so Python plugins need an interpreter already on the target machine'
}

$artefacts = @()
$sign = Join-Path $scriptDir 'sign.ps1'

if (-not $Unsigned) {
    # Fail before invoking WiX or makeappx when release credentials are
    # incomplete. Leaving an unsigned artefact behind after a late signing
    # failure is easy to mistake for a successful build.
    if (-not $CertificateThumbprint -and -not $CertificatePath) {
        Stop-WithError "no signing certificate: set CRIKEY_WINDOWS_CERT_THUMBPRINT or CRIKEY_WINDOWS_CERT_PATH (with CRIKEY_WINDOWS_CERT_PASSWORD), or pass -Unsigned to build artefacts that cannot be distributed."
    }
    if ($CertificatePath -and -not $CertificateThumbprint -and -not $env:CRIKEY_WINDOWS_CERT_PASSWORD) {
        Stop-WithError "CRIKEY_WINDOWS_CERT_PASSWORD is not set for -CertificatePath; it is the only accepted way to pass a PFX password"
    }
    if ($CertificateThumbprint) {
        $certificate = Get-ChildItem -Path "Cert:\CurrentUser\My\$CertificateThumbprint" -ErrorAction SilentlyContinue
        if (-not $certificate) {
            Stop-WithError "no certificate with thumbprint '$CertificateThumbprint' in Cert:\CurrentUser\My"
        }
    }

    # The payload is signed before it is packaged, not only the package. An
    # MSI signature covers the .msi file; it says nothing about the four
    # executables it lays down, so a signed MSI would still install a launcher
    # that raises SmartScreen's "Windows protected your PC" on first run, and
    # three unsigned supervised hosts beside it. Inside an MSIX the package
    # signature does cover the payload, but signing the same staged files once
    # here serves both formats and costs one pass.
    foreach ($executable in @('crikey.exe', 'crikey-launcher.exe', 'crikey-wasm-host.exe', 'crikey-cabi-host.exe')) {
        & $sign -Path (Join-Path $stage $executable) `
            -CertificateThumbprint $CertificateThumbprint `
            -CertificatePath $CertificatePath `
            -TimestampUrl $TimestampUrl
    }
}

if ($Format -eq 'msi' -or $Format -eq 'both') {
    # `wix` is the WiX v4 dotnet tool. It is deliberately not vendored: a
    # packaging toolchain installed by the packaging script is a toolchain
    # nobody pinned.
    $wix = Resolve-RequiredTool -Name 'wix' -ProvidedBy 'the WiX v4 toolset: dotnet tool install --global wix'

    $msi = Join-Path $OutputDirectory "CriKey-$Version-x64.msi"
    Write-Note "building $msi"

    # A previous run's MSI under this exact name must not survive a failure of
    # this one: only its timestamp would distinguish it, and sign.ps1 pointed
    # at that path would sign and verify a stale build without complaint.
    if (Test-Path -LiteralPath $msi) {
        Remove-Item -LiteralPath $msi -Force
    }

    $wixArgs = @(
        'build',
        (Join-Path $scriptDir 'crikey.wxs'),
        '-arch', 'x64',
        '-d', "Version=$Version",
        '-d', "StageDir=$stage",
        '-o', $msi
    )
    if ($havePythonRuntime) {
        $wixArgs += @('-d', 'PythonRuntime=1')
    }

    & $wix @wixArgs
    if ($LASTEXITCODE -ne 0) {
        Stop-WithError "wix build failed with exit code $LASTEXITCODE"
    }

    if ($Validate) {
        # `wix build` does not validate; the .NET tool exposes validation as a
        # separate subcommand, and only an MSBuild .wixproj runs it
        # automatically. Three ICEs describe a package this one cannot be, and
        # crikey.wxs records the reasoning per ICE: ICE38 and ICE64 are
        # per-user-profile rules that the harvested Python trees cannot satisfy
        # and that a package installing into exactly one profile does not need,
        # and ICE91 objects to per-user profile directories as such. Nothing
        # else is suppressed, so any other ICE is a real finding.
        Write-Note "validating $msi"
        & $wix msi validate '-sice' 'ICE38' '-sice' 'ICE64' '-sice' 'ICE91' $msi
        if ($LASTEXITCODE -ne 0) {
            Stop-WithError "wix msi validate failed with exit code $LASTEXITCODE"
        }
    }

    $artefacts += $msi
}

if ($Format -eq 'msix' -or $Format -eq 'both') {
    $makeappx = Get-Command 'makeappx.exe' -CommandType Application -ErrorAction SilentlyContinue
    if ($makeappx) {
        $makeappxPath = $makeappx.Source
    }
    else {
        $makeappxPath = Find-WindowsSdkTool -Name 'makeappx.exe'
    }
    if (-not $makeappxPath) {
        Stop-WithError "required tool 'makeappx.exe' was not found (provided by the Windows 10/11 SDK). Install the SDK's 'Windows SDK Signing Tools for Desktop Apps' component."
    }

    if (-not $MsixAssets) {
        Stop-WithError "the MSIX route needs -MsixAssets: a directory containing StoreLogo.png (50x50), Square150x150Logo.png (150x150) and Square44x44Logo.png (44x44). No artwork is checked into this repository, and referencing images that are not in the package produces a package that installs and then shows a blank tile."
    }

    foreach ($asset in @('StoreLogo.png', 'Square150x150Logo.png', 'Square44x44Logo.png')) {
        if (-not (Test-Path -LiteralPath (Join-Path $MsixAssets $asset))) {
            Stop-WithError "-MsixAssets directory '$MsixAssets' has no $asset"
        }
    }

    if (-not $MsixPublisher) {
        # Identity/@Publisher must equal the signing certificate's Subject
        # exactly, so it is read off the certificate rather than guessed.
        if ($CertificateThumbprint) {
            $certificate = Get-ChildItem -Path "Cert:\CurrentUser\My\$CertificateThumbprint" -ErrorAction SilentlyContinue
            if (-not $certificate) {
                Stop-WithError "no certificate with thumbprint '$CertificateThumbprint' in Cert:\CurrentUser\My, so the MSIX publisher cannot be derived; pass -MsixPublisher"
            }
            $MsixPublisher = $certificate.Subject
        }
        else {
            Stop-WithError "the MSIX route needs -MsixPublisher (the exact Subject of the signing certificate), or -CertificateThumbprint to derive it from. A package whose Publisher does not match its signature cannot be installed."
        }
    }

    $assetTarget = Join-Path $stage 'Assets'
    New-Item -ItemType Directory -Path $assetTarget -Force | Out-Null
    Copy-Item -Path (Join-Path $MsixAssets '*.png') -Destination $assetTarget -Force

    # MSIX versions are four-part; Cargo's are three.
    $msixVersion = "$Version.0"
    # Both values land inside XML attributes. A certificate Subject legally
    # contains characters XML does not -- `CN=Example & Sons Ltd` is an
    # ordinary company name -- and a bare `&` or `"` written into
    # Identity/@Publisher makes makeappx fail with a parse error pointing at a
    # generated file, saying nothing about the certificate. Escaping is exactly
    # right here even though Publisher must match the certificate
    # byte-for-byte: the XML parser hands makeappx the original bytes back.
    $manifest = Get-Content -LiteralPath (Join-Path $scriptDir 'AppxManifest.xml') -Raw
    $manifest = $manifest.Replace('__CRIKEY_MSIX_PUBLISHER__', [System.Security.SecurityElement]::Escape($MsixPublisher))
    $manifest = $manifest.Replace('__CRIKEY_MSIX_VERSION__', [System.Security.SecurityElement]::Escape($msixVersion))
    Set-Content -LiteralPath (Join-Path $stage 'AppxManifest.xml') -Value $manifest -Encoding UTF8

    $msix = Join-Path $OutputDirectory "CriKey-$Version-x64.msix"
    Write-Note "building $msix"
    if (Test-Path -LiteralPath $msix) {
        Remove-Item -LiteralPath $msix -Force
    }

    & $makeappxPath pack /d $stage /p $msix /o
    if ($LASTEXITCODE -ne 0) {
        Stop-WithError "makeappx pack failed with exit code $LASTEXITCODE"
    }
    $artefacts += $msix
}

if ($Unsigned) {
    Write-Note 'built UNSIGNED:'
    foreach ($artefact in $artefacts) {
        Write-Note "  $artefact"
    }
    Write-Note 'an unsigned MSIX cannot be installed at all, and an unsigned MSI warns about an unknown publisher'
    return
}


foreach ($artefact in $artefacts) {
    & $sign -Path $artefact `
        -CertificateThumbprint $CertificateThumbprint `
        -CertificatePath $CertificatePath `
        -TimestampUrl $TimestampUrl
}

Write-Note 'built and signed:'
foreach ($artefact in $artefacts) {
    Write-Note "  $artefact"
}
