# Check a SecurePlan CAD MSI (DSK-04, DSK-05): per-user with no elevation,
# SecurePlan CAD's own UpgradeCode, no file associations, and the
# secureplan-cad: scheme registered under HKCU\Software\Classes pointing at
# the installed executable. -Version also checks the ProductVersion.
# Everything read is printed first, so a failure shows what the MSI holds.
#
#   packaging/windows/check-msi.ps1 -Msi <file.msi> [-Version <X.Y.Z>]
param(
  [Parameter(Mandatory = $true)] [string] $Msi,
  [string] $Version
)
$ErrorActionPreference = 'Stop'
$installer = New-Object -ComObject WindowsInstaller.Installer
$db = $installer.GetType().InvokeMember('OpenDatabase', 'InvokeMethod', $null, $installer, @((Resolve-Path $Msi).Path, 0))

# The rows of $sql as strings, each row's fields joined by '|'. Always a
# string array (possibly empty). The COM calls that return nothing are cast
# to [void]: an unassigned $null would otherwise join the function's output.
function Query([string] $sql, [int] $fields = 1) {
  $view = $db.GetType().InvokeMember('OpenView', 'InvokeMethod', $null, $db, @($sql))
  [void] $view.GetType().InvokeMember('Execute', 'InvokeMethod', $null, $view, $null)
  $rows = [System.Collections.Generic.List[string]]::new()
  while ($true) {
    $record = $view.GetType().InvokeMember('Fetch', 'InvokeMethod', $null, $view, $null)
    if ($null -eq $record) { break }
    $values = [System.Collections.Generic.List[string]]::new()
    for ($i = 1; $i -le $fields; $i++) {
      $values.Add([string] $record.GetType().InvokeMember('StringData', 'GetProperty', $null, $record, @($i)))
    }
    $rows.Add($values -join '|')
  }
  [void] $view.GetType().InvokeMember('Close', 'InvokeMethod', $null, $view, $null)
  return , $rows.ToArray()
}

# The value of a Property-table property, or $null when it is absent.
function Property([string] $name) {
  [string[]] $rows = Query "SELECT ``Value`` FROM ``Property`` WHERE ``Property``='$name'"
  if ($rows.Count -eq 0) { return $null }
  return $rows[0]
}

# ── Read ───────────────────────────────────────────────────────────────────
$allUsers = Property 'ALLUSERS'
$msiInstallPerUser = Property 'MSIINSTALLPERUSER'
$upgrade = Property 'UpgradeCode'
$productVersion = Property 'ProductVersion'
$summary = $db.GetType().InvokeMember('SummaryInformation', 'GetProperty', $null, $db, @(0))
$wordCount = [int] $summary.GetType().InvokeMember('Property', 'GetProperty', $null, $summary, @(15))
$associationTables = @('Extension', 'ProgId', 'Verb', 'MIME', 'TypeLib') | Where-Object {
  ([string[]] (Query "SELECT ``Name`` FROM ``_Tables`` WHERE ``Name``='$_'")).Count -gt 0
}
# Registry rows: Root|Key|Name|Value. Root 1 is HKEY_CURRENT_USER. KEY is an
# MSI SQL keyword, so every name is back-quoted.
[string[]] $registry = Query 'SELECT `Root`, `Key`, `Name`, `Value` FROM `Registry`' 4
[string[]] $classes = @($registry | Where-Object { ($_ -split '\|')[1] -like 'Software\Classes*' })

Write-Host "MSI: $Msi"
Write-Host "  ALLUSERS: $(if ($null -eq $allUsers) { '(absent)' } else { "'$allUsers'" })"
Write-Host "  MSIINSTALLPERUSER: $(if ($null -eq $msiInstallPerUser) { '(absent)' } else { "'$msiInstallPerUser'" })"
Write-Host "  Summary word count: $wordCount (bit 3, no elevation: $(($wordCount -band 8) -ne 0))"
Write-Host "  UpgradeCode: $upgrade"
Write-Host "  ProductVersion: $productVersion"
Write-Host "  File-association tables: $(if ($associationTables) { $associationTables -join ', ' } else { '(none)' })"
Write-Host "  Software\Classes rows ($($classes.Count)):"
foreach ($row in $classes) { Write-Host "    $row" }

# ── Check ──────────────────────────────────────────────────────────────────
$problems = [System.Collections.Generic.List[string]]::new()
# Per-user: ALLUSERS absent (or empty), and no elevation needed.
if (-not [string]::IsNullOrEmpty($allUsers)) { $problems.Add("ALLUSERS is '$allUsers': the MSI is not per-user") }
if (($wordCount -band 8) -eq 0) { $problems.Add('the MSI requires elevation') }
if ($upgrade -ne '{6AB8E75D-B609-40B8-B5A5-2BF8697CC6DE}') { $problems.Add("unexpected UpgradeCode '$upgrade'") }
if ($Version -and $productVersion -ne $Version) { $problems.Add("ProductVersion is '$productVersion', expected '$Version'") }
foreach ($table in $associationTables) { $problems.Add("the MSI has a $table table (file association)") }
foreach ($row in $classes) {
  $root, $key = ($row -split '\|')[0, 1]
  if ($root -ne '1') { $problems.Add("a Software\Classes entry is not per-user: $row") }
  if ($key -ne 'Software\Classes\secureplan-cad' -and $key -notlike 'Software\Classes\secureplan-cad\*') {
    $problems.Add("the MSI registers something other than the secureplan-cad: scheme: $row")
  }
}
$expected = @(
  '1|Software\Classes\secureplan-cad||URL:SecurePlan CAD',
  '1|Software\Classes\secureplan-cad\shell\open\command||"[APPLICATIONROOTDIRECTORY]SecurePlanCAD.exe" "%1"'
)
foreach ($row in $expected) {
  if ($classes -notcontains $row) { $problems.Add("missing scheme registration: $row") }
}
if (-not ($classes | Where-Object { $_.StartsWith('1|Software\Classes\secureplan-cad|URL Protocol|') })) {
  $problems.Add('missing the URL Protocol value')
}

if ($problems.Count -gt 0) {
  foreach ($problem in $problems) { Write-Host "::error::$problem" }
  throw "The MSI check failed: $($problems -join '; ')"
}
Write-Host "MSI checks passed: per-user, no file associations, secureplan-cad: registered for the current user."
