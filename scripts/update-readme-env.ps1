<#
.SYNOPSIS
  Regenerates the "Verified environment" section of README.md from `onchip probe --json`.
  Never edit that section by hand.
#>
param(
  [string]$Readme = (Join-Path $PSScriptRoot "..\README.md"),
  [string]$Onchip = "onchip"
)
$ErrorActionPreference = "Stop"

$json = & $Onchip probe --json | ConvertFrom-Json
$date = Get-Date -Format "yyyy-MM-dd"

$lines = @()
$lines += "| Field | Value |"
$lines += "|---|---|"
$lines += "| Date | $date |"
$lines += "| OS | $($json.os) |"
$lines += "| Adapter | $($json.adapter.name) |"
$lines += "| Backend | $($json.adapter.backend) |"
$lines += "| Driver | $($json.adapter.driver) $($json.adapter.driver_info) |"
$lines += "| Unified memory | $($json.adapter.unified_memory) |"
if ($json.hw_decoders) { $lines += "| HW decoders | $(($json.hw_decoders | ForEach-Object { $_.name }) -join ', ') |" }
if ($json.hw_encoders) { $lines += "| HW encoders | $(($json.hw_encoders | ForEach-Object { $_.name }) -join ', ') |" }
$block = ($lines -join "`n")

$content = Get-Content -Raw $Readme
$pattern = "(?s)(<!-- onchip:env:begin -->).*?(<!-- onchip:env:end -->)"
$new = [regex]::Replace($content, $pattern, "`$1`n$block`n`$2")
Set-Content -NoNewline -Path $Readme -Value $new
Write-Host "README.md environment section updated."
