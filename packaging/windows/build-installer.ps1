param(
    [Parameter(Mandatory = $true)][string]$Stage,
    [Parameter(Mandatory = $true)][string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
$Stage = (Resolve-Path -LiteralPath $Stage).Path
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$OutputDirectory = (Resolve-Path -LiteralPath $OutputDirectory).Path

foreach ($relative in @('openmouse-bridge.exe', 'README.md', 'DM-Sans-OFL.txt', 'native-hid/node/node.exe', 'native-hid/src/apply.mjs')) {
    if (-not (Test-Path -LiteralPath (Join-Path $Stage $relative) -PathType Leaf)) {
        throw "Installer payload is missing $relative"
    }
}

$metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Could not read Cargo package metadata' }
$version = ($metadata.packages | Where-Object { $_.name -eq 'openmouse-bridge' } | Select-Object -First 1).version
if (-not $version) { throw 'Could not determine Bridge version' }

$compiler = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
if (-not $compiler) {
    $compiler = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
}
if (-not (Test-Path -LiteralPath $compiler -PathType Leaf)) { throw 'Inno Setup 6 compiler is missing' }

& $compiler "/DSourceDir=$Stage" "/DAppVersion=$version" "/O$OutputDirectory" 'packaging/windows/bridge.iss'
if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compilation failed' }
$installer = Join-Path $OutputDirectory 'OpenMouse-Bridge-windows-x64-setup.exe'
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) { throw 'Installer output is missing' }

$verify = Join-Path $env:RUNNER_TEMP 'openmouse-installer-verify'
New-Item -ItemType Directory -Force -Path $verify | Out-Null
$installed = Join-Path $verify 'OpenMouse Bridge'
$installLog = Join-Path $verify 'install.log'
$process = Start-Process -FilePath $installer -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/DIR=`"$installed`"", "/LOG=`"$installLog`"") -PassThru -Wait
if ($process.ExitCode -ne 0) { throw "Installer smoke test failed with exit code $($process.ExitCode)" }
foreach ($relative in @('openmouse-bridge.exe', 'native-hid/node/node.exe', 'native-hid/src/apply.mjs', 'DM-Sans-OFL.txt', 'unins000.exe')) {
    if (-not (Test-Path -LiteralPath (Join-Path $installed $relative) -PathType Leaf)) {
        throw "Installer smoke test is missing $relative"
    }
}
$uninstaller = Join-Path $installed 'unins000.exe'
$process = Start-Process -FilePath $uninstaller -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART') -PassThru -Wait
if ($process.ExitCode -ne 0) { throw "Uninstaller smoke test failed with exit code $($process.ExitCode)" }
if (Test-Path -LiteralPath (Join-Path $installed 'openmouse-bridge.exe')) {
    throw 'Uninstaller left the Bridge executable behind'
}

(Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLower() + '  OpenMouse-Bridge-windows-x64-setup.exe' |
    Set-Content -Path "$installer.sha256"
