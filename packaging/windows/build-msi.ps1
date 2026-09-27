# Build the per-user SecurePlan CAD MSI (DSK-05) from a SecurePlan CAD
# executable, with WiX Toolset 3 (the WIX environment variable, as on the
# windows-2022 runners). Used by the release workflow and SecurePlan CI.
#
#   packaging/windows/build-msi.ps1 -Exe <OpenCADStudio.exe> -Version <X.Y.Z> -Out <file.msi>
param(
  [Parameter(Mandatory = $true)] [string] $Exe,
  [Parameter(Mandatory = $true)] [string] $Version,
  [Parameter(Mandatory = $true)] [string] $Out
)
$ErrorActionPreference = 'Stop'
if ($Version -notmatch '^(0|[1-9][0-9]{0,2})\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' -or [int]$Matches[1] -gt 255) {
  throw "Version must be MAJOR.MINOR.PATCH with MAJOR <= 255 (got '$Version')"
}
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$windows = Join-Path $root 'packaging\windows'
# The Vizmo icon (DSK-08), made by packaging/secureplan/make-icons.py.
$icon = Join-Path $root 'packaging\secureplan\AppIcon.ico'
$candle = Join-Path $env:WIX 'bin\candle.exe'
$light = Join-Path $env:WIX 'bin\light.exe'
$objects = Join-Path ([IO.Path]::GetTempPath()) ('secureplan-msi-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $objects | Out-Null
& $candle -nologo -arch x64 "-dVersion=$Version" `
  "-dSource=$((Resolve-Path $Exe).Path)" `
  "-dIcon=$icon" `
  "-dLicense=$(Join-Path $windows 'License.rtf')" `
  (Join-Path $windows 'main.wxs') (Join-Path $windows 'ui.wxs') -out "$objects\"
if ($LASTEXITCODE -ne 0) { throw 'candle failed' }
& $light -nologo (Join-Path $objects 'main.wixobj') (Join-Path $objects 'ui.wixobj') `
  -ext WixUIExtension -cultures:en-us -loc (Join-Path $windows 'strings.en-US.wxl') -out $Out
if ($LASTEXITCODE -ne 0) { throw 'light failed' }
Remove-Item -Recurse -Force $objects
Write-Host "Built $Out (SecurePlan CAD $Version, per-user)."
