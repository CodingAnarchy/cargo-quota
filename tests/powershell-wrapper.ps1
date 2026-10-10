# Checks the PowerShell wrapper exactly as the README documents it.
# Usage: powershell-wrapper.ps1 -Readme README.md -Project <path to a cargo project>
param([string]$Readme, [string]$Project)
$ErrorActionPreference = 'Continue'
$snippet = [regex]::Match((Get-Content $Readme -Raw), '(?s)```powershell\r?\n(.*?)```').Groups[1].Value
if (-not $snippet) { throw 'no powershell block in README' }
Invoke-Expression $snippet
Set-Location $Project
$env:CARGO_QUOTA_PERCENT = '0.0000001'

$out = cargo build 2>&1 | Out-String
if ($out -notmatch 'Pruning') { throw "build did not prune:`n$out" }
$out = cargo +stable build 2>&1 | Out-String
if ($out -notmatch 'Pruning') { throw "+stable build did not prune:`n$out" }
$out = cargo --version 2>&1 | Out-String
if ($out -notmatch '^cargo \d') { throw "--version not passed through:`n$out" }
$out = cargo metadata --format-version 1 --no-deps 2>&1 | Out-String
if ($out -match 'Pruning') { throw "metadata should not prune" }
$out = cargo run -q -- 'a b' 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) { throw "run failed:`n$out" }
cargo build --definitely-not-a-flag 2>&1 | Out-Null
if ($LASTEXITCODE -eq 0) { throw 'exit code not propagated' }
'wrapper OK'
exit 0
