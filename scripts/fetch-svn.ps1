<#
.SYNOPSIS
  Vendors a Windows svn client into the embedded-assets directory so the server
  ships a self-contained SVN backend.

.DESCRIPTION
  Downloads a SlikSVN zip distribution, extracts `svn.exe` and its DLLs, and
  copies them to crates/mortis-embed/assets/svn/windows-x86_64/. After running,
  rebuild the project; the binaries are embedded automatically.

  SlikSVN does not publish a stable zip URL, so pass -Url with the current
  download (see https://sliksvn.com/download/). As a fallback this script can
  bundle a system-installed svn (-FromSystem).

.EXAMPLE
  ./scripts/fetch-svn.ps1 -Url https://.../sliksvn.zip
  ./scripts/fetch-svn.ps1 -FromSystem
#>
param(
    [string]$Url,
    [switch]$FromSystem
)

$ErrorActionPreference = "Stop"
$dest = Join-Path $PSScriptRoot "../crates/mortis-embed/assets/svn/windows-x86_64"
New-Item -ItemType Directory -Force -Path $dest | Out-Null

if ($FromSystem) {
    $svn = (Get-Command svn -ErrorAction Stop).Source
    $binDir = Split-Path $svn
    Write-Host "Bundling system svn from $binDir"
    Copy-Item (Join-Path $binDir "svn.exe") $dest -Force
    # Copy DLLs that live next to svn.exe (APR, serf, sqlite, openssl, etc.)
    Get-ChildItem $binDir -Filter *.dll | Copy-Item -Destination $dest -Force
    Write-Host "Done -> $dest"
    return
}

if (-not $Url) {
    throw "Provide -Url <sliksvn zip url> or use -FromSystem. See https://sliksvn.com/download/"
}

$tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP "mortis-svn-fetch")
$zip = Join-Path $tmp "sliksvn.zip"
Write-Host "Downloading $Url"
Invoke-WebRequest -Uri $Url -OutFile $zip
Expand-Archive -Path $zip -DestinationPath $tmp -Force

$svnExe = Get-ChildItem $tmp -Recurse -Filter svn.exe | Select-Object -First 1
if (-not $svnExe) { throw "svn.exe not found in archive" }
$binDir = Split-Path $svnExe.FullName
Copy-Item (Join-Path $binDir "svn.exe") $dest -Force
Get-ChildItem $binDir -Filter *.dll | Copy-Item -Destination $dest -Force
Write-Host "Done -> $dest"
