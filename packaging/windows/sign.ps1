<#
.SYNOPSIS
    Authenticode-sign a CriKey MSI or MSIX and verify the result.

.DESCRIPTION
    The certificate comes from the environment or from the command line, never
    from this repository. Nothing certificate-shaped is checked in, and nothing
    in this script writes a certificate, a thumbprint or a password to a file
    or to the log.

    Two ways to supply the certificate, in this order of preference:

      1. -CertificateThumbprint, or $env:CRIKEY_WINDOWS_CERT_THUMBPRINT. The
         private key stays in the certificate store (or on the HSM or token
         backing it) and never becomes a file. This is what a signing machine
         should use.

      2. -CertificatePath, or $env:CRIKEY_WINDOWS_CERT_PATH, with the password
         in $env:CRIKEY_WINDOWS_CERT_PASSWORD. The password is read from the
         environment only. It is never a parameter of this script and never an
         argument of signtool either: signtool's /f /p interface would put it
         on a command line, and command lines are readable by every process on
         the machine. The PFX is imported into Cert:\CurrentUser\My for the
         duration of the signature, signed by thumbprint like route 1, and
         removed again whether the signature succeeded or failed.

    Every signature is timestamped. Without a timestamp the signature stops
    validating on the day the certificate expires, and every copy already
    downloaded becomes untrusted with it.

    Every signature is verified immediately after it is applied, with
    `signtool verify /pa`. That check is not optional and there is no switch
    here that skips it, weakens it, or disables signature verification
    anywhere on the machine. A signature nobody checked is a signature nobody
    can rely on, and the failure mode -- an artefact that installs on the build
    machine and nowhere else -- is discovered far too late.

    THIS SCRIPT REQUIRES A REAL WINDOWS HOST: signtool.exe ships with the
    Windows SDK and has no cross-platform substitute.

.PARAMETER Path
    The file to sign: an .msi, an .msix, or one of the executables build.ps1
    stages into a package.

.PARAMETER CertificateThumbprint
    SHA-1 thumbprint of a code-signing certificate in Cert:\CurrentUser\My.
    Defaults to $env:CRIKEY_WINDOWS_CERT_THUMBPRINT.

.PARAMETER CertificatePath
    Path to a PFX file. Used only when no thumbprint is given. Defaults to
    $env:CRIKEY_WINDOWS_CERT_PATH; the password comes from
    $env:CRIKEY_WINDOWS_CERT_PASSWORD.

.PARAMETER TimestampUrl
    RFC 3161 timestamp authority.

.EXAMPLE
    $env:CRIKEY_WINDOWS_CERT_THUMBPRINT = 'A1B2...'
    .\packaging\windows\sign.ps1 -Path .\CriKey-<version>-x64.msi
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $Path,
    [string] $CertificateThumbprint = $env:CRIKEY_WINDOWS_CERT_THUMBPRINT,
    [string] $CertificatePath = $env:CRIKEY_WINDOWS_CERT_PATH,
    [string] $TimestampUrl = 'http://timestamp.digicert.com'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$program = 'crikey-packaging(windows/sign)'

function Stop-WithError {
    param([string] $Message)
    throw "${program}: error: $Message"
}

function Write-Note {
    param([string] $Message)
    Write-Host "${program}: $Message"
}

function Find-Signtool {
    $onPath = Get-Command 'signtool.exe' -CommandType Application -ErrorAction SilentlyContinue
    if ($onPath) {
        return $onPath.Source
    }

    # The SDK does not put signtool on PATH; it lives under
    # `Windows Kits\10\bin\<sdk version>\x64\`, and a machine that ever carried
    # an older SDK also has a legacy unversioned `bin\x64\` beside the
    # versioned ones. Sorting the full paths as text puts `\bin\x64\` first,
    # because 'x' sorts above '1', so a plain descending sort picks the oldest
    # signtool on exactly the machines that have one -- and an old signtool
    # does not recognise .msix. The version segment is parsed and compared as a
    # version instead, and the unversioned directory is only a fallback.
    $roots = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\bin",
        "$env:ProgramFiles\Windows Kits\10\bin"
    )
    $candidates = @()
    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root)) {
            continue
        }
        $candidates += Get-ChildItem -Path $root -Filter 'signtool.exe' -Recurse -ErrorAction SilentlyContinue |
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

if (-not (Test-Path -LiteralPath $Path)) {
    Stop-WithError "'$Path' does not exist"
}

$signtool = Find-Signtool
if (-not $signtool) {
    Stop-WithError "required tool 'signtool.exe' was not found (provided by the Windows 10/11 SDK component 'Windows SDK Signing Tools for Desktop Apps'). This script only runs on Windows."
}

# /fd SHA256 is the file digest; /td SHA256 is the timestamp digest. Both are
# stated explicitly because signtool's defaults have changed between SDK
# versions, and a SHA-1 file digest is rejected outright by current Windows.
$common = @('/fd', 'SHA256', '/tr', $TimestampUrl, '/td', 'SHA256', '/v')

if ($CertificateThumbprint) {
    Write-Note "signing $Path with the certificate in the store"
    & $signtool sign @common '/sha1' $CertificateThumbprint $Path
}
elseif ($CertificatePath) {
    if (-not (Test-Path -LiteralPath $CertificatePath)) {
        Stop-WithError "certificate file '$CertificatePath' does not exist"
    }
    $password = $env:CRIKEY_WINDOWS_CERT_PASSWORD
    if (-not $password) {
        Stop-WithError "CRIKEY_WINDOWS_CERT_PASSWORD is not set; it is the only accepted way to pass a PFX password, because command-line arguments are readable by every process on the machine"
    }

    # signtool's own /f /p interface would put that password straight back on a
    # command line, where Win32_Process.CommandLine, NtQueryInformationProcess
    # and every process-creation telemetry pipeline on the machine can read it.
    # The PFX is instead imported into this user's certificate store for the
    # duration of the signature and signed by thumbprint, exactly as the
    # preferred route does, and removed again in a finally block so a failed
    # signature does not leave a private key behind. A PFX carrying a chain
    # imports its intermediates too; every thumbprint imported here is removed.
    Write-Note "signing $Path with $CertificatePath"
    $securePassword = ConvertTo-SecureString -String $password -AsPlainText -Force
    $imported = @(Import-PfxCertificate -FilePath $CertificatePath -CertStoreLocation 'Cert:\CurrentUser\My' -Password $securePassword)
    try {
        $signing = @($imported | Where-Object { $_.HasPrivateKey })
        if ($signing.Count -ne 1) {
            Stop-WithError "'$CertificatePath' holds $($signing.Count) certificates with a private key; exactly one is needed to sign with"
        }
        & $signtool sign @common '/sha1' $signing[0].Thumbprint $Path
    }
    finally {
        foreach ($certificate in $imported) {
            Remove-Item -LiteralPath "Cert:\CurrentUser\My\$($certificate.Thumbprint)" -Force -ErrorAction SilentlyContinue
        }
    }
}
else {
    Stop-WithError "no certificate: set CRIKEY_WINDOWS_CERT_THUMBPRINT, or CRIKEY_WINDOWS_CERT_PATH together with CRIKEY_WINDOWS_CERT_PASSWORD"
}

if ($LASTEXITCODE -ne 0) {
    Stop-WithError "signtool sign failed with exit code $LASTEXITCODE"
}

# /pa selects the Authenticode policy, which is the policy Windows itself
# applies when the user runs the installer. Verifying under the default
# (driver) policy would pass for something the user's machine will still
# refuse.
Write-Note "verifying the signature on $Path"
& $signtool verify /pa /v $Path
if ($LASTEXITCODE -ne 0) {
    Stop-WithError "signtool verify failed with exit code $LASTEXITCODE; the artefact is signed but the signature does not validate"
}

Write-Note "signed and verified: $Path"
