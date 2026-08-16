# OpenMouse Attack Shark X11 — self-sign and install the WinUSB driver package.
#
# For a machine you control (testing). It creates a free self-signed
# certificate, tells Windows to trust it, builds and signs the driver catalog,
# and installs the package. No paid certificate and no Windows Driver Kit are
# required — everything here is built into Windows/PowerShell, with the WDK's
# inf2cat used automatically if it happens to be installed.
#
# Run from an ADMIN PowerShell:  powershell -ExecutionPolicy Bypass -File .\sign-and-install.ps1
#
# It is safe and reversible: no firmware is written, only interface 2 of the
# known Attack Shark PIDs is affected, and .\uninstall.ps1 (or Device Manager)
# reverts it. See README.md.

#Requires -RunAsAdministrator
$ErrorActionPreference = 'Stop'

$here    = Split-Path -Parent $MyInvocation.MyCommand.Path
$inf     = Join-Path $here 'OpenMouse-AttackShark-X11.inf'
$cat     = Join-Path $here 'OpenMouse-AttackShark-X11.cat'
$subject = 'CN=OpenMouse Test Signing'

if (-not (Test-Path $inf)) { throw "Cannot find $inf" }

# Record a transcript the Bridge can read back if the install fails.
$transcript = Join-Path $env:TEMP 'openmouse-driver.log'
Start-Transcript -Path $transcript -Force -ErrorAction SilentlyContinue | Out-Null

Write-Host '[1/6] Creating or reusing the self-signed certificate...'
$cert = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { $_.Subject -eq $subject } | Select-Object -First 1
if (-not $cert) {
    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject `
        -CertStoreLocation Cert:\CurrentUser\My -KeyUsage DigitalSignature `
        -NotAfter (Get-Date).AddYears(5)
}
Write-Host "      thumbprint $($cert.Thumbprint)"

Write-Host '[2/6] Trusting the certificate (Trusted Root + Trusted Publishers)...'
$cer = Join-Path $env:TEMP 'openmouse-test.cer'
Export-Certificate -Cert $cert -FilePath $cer | Out-Null
Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null
Remove-Item $cer -Force

Write-Host '[3/6] Building the driver catalog...'
if (Test-Path $cat) { Remove-Item $cat -Force }
$inf2cat = Get-Command inf2cat.exe -ErrorAction SilentlyContinue
if ($inf2cat) {
    Write-Host '      using WDK inf2cat'
    & inf2cat.exe /driver:"$here" /os:10_X64,10_ARM64
} else {
    Write-Host '      using built-in New-FileCatalog'
    New-FileCatalog -Path $inf -CatalogFilePath $cat -CatalogVersion 2 | Out-Null
}

Write-Host '[4/6] Signing the catalog...'
$result = Set-AuthenticodeSignature -FilePath $cat -Certificate $cert
if ($result.Status -ne 'Valid') { throw "Signing failed: $($result.StatusMessage)" }

Write-Host '[5/6] Adding the driver to the store (interface 2 of the Attack Shark X11 only)...'
& pnputil /add-driver "$inf" /install
$code = $LASTEXITCODE
if ($code -ne 0 -and $code -ne 3010 -and $code -ne 259) {
    throw "pnputil failed with exit code $code. See README.md."
}

Write-Host '[6/6] Re-binding interface 2 so Windows applies the new driver...'
# Adding a package stages it, but the interface keeps its old (HID) driver until
# it re-enumerates. Restart just the Attack Shark config interface (VID_1D57,
# MI_02) so Windows re-evaluates and binds WinUSB. This only touches interface 2
# (the config interface); the pointer/keyboard interfaces are untouched.
$dev = Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
    Where-Object { $_.InstanceId -match 'VID_1D57' -and $_.InstanceId -match 'MI_0?2($|\\)' }
if ($dev) {
    foreach ($d in $dev) {
        Write-Host "      restarting $($d.InstanceId)"
        Disable-PnpDevice -InstanceId $d.InstanceId -Confirm:$false -ErrorAction SilentlyContinue
    }
    Start-Sleep -Seconds 1
    foreach ($d in $dev) {
        Enable-PnpDevice -InstanceId $d.InstanceId -Confirm:$false -ErrorAction SilentlyContinue
    }
    Write-Host '      done'
} else {
    Write-Host '      interface 2 not found as a present device; unplug and replug the mouse instead.'
}

Stop-Transcript -ErrorAction SilentlyContinue | Out-Null

Write-Host ''
Write-Host 'Done. Check OpenMouse (Interface settings -> Bridge -> Native devices);'
Write-Host 'the X11 should now show as controllable. If not, unplug and replug it once.'
Write-Host 'To revert: run .\uninstall.ps1 from an admin PowerShell.'
