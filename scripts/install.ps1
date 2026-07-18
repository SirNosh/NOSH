param([switch]$NoPath)
$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
corepack pnpm --dir $root install --frozen-lockfile
corepack pnpm --dir $root build
$bin = Join-Path $env:LOCALAPPDATA "NOSH\bin"
New-Item -ItemType Directory -Force -Path $bin | Out-Null
$wrapper = Join-Path $bin "nosh.cmd"
Set-Content -LiteralPath $wrapper -Encoding ASCII -Value "@echo off`r`nnode `"$root\apps\cli\dist\main.js`" %*`r`n"
if (-not $NoPath) { $path = [Environment]::GetEnvironmentVariable("Path", "User"); if (($path -split ";") -notcontains $bin) { [Environment]::SetEnvironmentVariable("Path", (($path.TrimEnd(";") + ";" + $bin).TrimStart(";")), "User") } }
& schtasks.exe /Create /F /SC ONLOGON /RL LIMITED /TN "NOSH noshd" /TR "`"$wrapper`" start" | Out-Null
Write-Host "Installed NOSH $((Get-Content (Join-Path $root 'package.json') | ConvertFrom-Json).version). Open a new terminal and run: nosh setup"
