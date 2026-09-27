# Check a SecurePlan CAD MSI (DSK-04, DSK-05): per-user with no elevation,
# SecurePlan CAD's own UpgradeCode, no file associations, and the
# secureplan-cad: scheme registered under HKCU\Software\Classes pointing at
# the installed executable. -Version also checks the ProductVersion.
#
#   packaging/windows/check-msi.ps1 -Msi <file.msi> [-Version <X.Y.Z>]
param(
  [Parameter(Mandatory = $true)] [string] $Msi,
  [string] $Version
)
$ErrorActionPreference = 'Stop'
$installer = New-Object -ComObject WindowsInstaller.Installer
$db = $installer.GetType().InvokeMember('OpenDatabase', 'InvokeMethod', $null, $installer, @((Resolve-Path $Msi).Path, 0))

# Rows of $sql, each as its fields joined by '|'.
function Query([string] $sql, [int] $fields = 1) {
  $view = $db.GetType().InvokeMember('OpenView', 'InvokeMethod', $null, $db, @($sql))
  $view.GetType().InvokeMember('Execute', 'InvokeMethod', $null, $view, $null)
  $rows = @()
  while ($record = $view.GetType().InvokeMember('Fetch', 'InvokeMethod', $null, $view, $null)) {
    $values = @()
    for ($i = 1; $i -le $fields; $i++) {
      $values += $record.GetType().InvokeMember('StringData', 'GetProperty', $null, $record, $i)
    }
    $rows += ($values -join '|')
  }
  $view.GetType().InvokeMember('Close', 'InvokeMethod', $null, $view, $null)
  , $rows
}

$allUsers = Query "SELECT Value FROM Property WHERE Property='ALLUSERS'"
if ($allUsers.Count -gt 0) { throw "ALLUSERS is set ($allUsers): the MSI is not per-user" }
# Summary information word count bit 3: no elevation needed.
$summary = $db.GetType().InvokeMember('SummaryInformation', 'GetProperty', $null, $db, @(0))
$wordCount = $summary.GetType().InvokeMember('Property', 'GetProperty', $null, $summary, @(15))
if (($wordCount -band 8) -eq 0) { throw 'the MSI requires elevation' }
$upgrade = (Query "SELECT Value FROM Property WHERE Property='UpgradeCode'") -join ''
if ($upgrade -ne '{6AB8E75D-B609-40B8-B5A5-2BF8697CC6DE}') { throw "unexpected UpgradeCode $upgrade" }
if ($Version) {
  $productVersion = (Query "SELECT Value FROM Property WHERE Property='ProductVersion'") -join ''
  if ($productVersion -ne $Version) { throw "ProductVersion is $productVersion, expected $Version" }
}
foreach ($table in 'Extension', 'ProgId', 'Verb', 'MIME', 'TypeLib') {
  if ((Query "SELECT Name FROM _Tables WHERE Name='$table'").Count -gt 0) { throw "the MSI has a $table table (file association)" }
}

# Registry rows: Root|Key|Name|Value. Root 1 is HKEY_CURRENT_USER.
# KEY is an MSI SQL keyword, so the names are back-quoted.
$registry = Query 'SELECT `Root`, `Key`, `Name`, `Value` FROM `Registry`' 4
$classes = $registry | Where-Object { ($_ -split '\|')[1] -like 'Software\Classes*' }
foreach ($row in $classes) {
  $root, $key = ($row -split '\|')[0, 1]
  if ($root -ne '1') { throw "a Software\Classes entry is not per-user: $row" }
  if ($key -ne 'Software\Classes\secureplan-cad' -and $key -notlike 'Software\Classes\secureplan-cad\*') {
    throw "the MSI registers something other than the secureplan-cad: scheme: $row"
  }
}
$expected = @(
  '1|Software\Classes\secureplan-cad||URL:SecurePlan CAD',
  '1|Software\Classes\secureplan-cad\shell\open\command||"[APPLICATIONROOTDIRECTORY]SecurePlanCAD.exe" "%1"'
)
foreach ($row in $expected) {
  if ($classes -notcontains $row) { throw "missing scheme registration: $row (have: $($classes -join '; '))" }
}
if (-not ($classes | Where-Object { $_ -like '1|Software\Classes\secureplan-cad|URL Protocol|*' })) {
  throw 'missing the URL Protocol value'
}
Write-Host "MSI checks passed: per-user, no file associations, secureplan-cad: registered for the current user."
