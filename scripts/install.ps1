param([switch]$NoPath, [switch]$NoScheduledTask, [switch]$SkipBuild, [string]$BinRoot)
$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if (-not $SkipBuild) {
  corepack pnpm --dir $root install --frozen-lockfile
  if ($LASTEXITCODE -ne 0) { throw "Dependency installation failed ($LASTEXITCODE)" }
  corepack pnpm --dir $root build
  if ($LASTEXITCODE -ne 0) { throw "Build failed ($LASTEXITCODE)" }
}
$bin = if ($BinRoot) { (New-Item -ItemType Directory -Force -Path $BinRoot).FullName } else { Join-Path $env:LOCALAPPDATA "NOSH\bin" }
New-Item -ItemType Directory -Force -Path $bin | Out-Null
$wrapper = Join-Path $bin "nosh.cmd"
Set-Content -LiteralPath $wrapper -Encoding ASCII -Value "@echo off`r`nnode `"$root\apps\cli\dist\main.js`" %*`r`n"
if (-not $NoPath) { $path = [Environment]::GetEnvironmentVariable("Path", "User"); if (($path -split ";") -notcontains $bin) { [Environment]::SetEnvironmentVariable("Path", (($path.TrimEnd(";") + ";" + $bin).TrimStart(";")), "User") } }
if (-not $NoScheduledTask) { & schtasks.exe /Create /F /SC ONLOGON /RL LIMITED /TN "NOSH noshd" /TR "`"$wrapper`" start" | Out-Null }
Write-Host "Installed NOSH $((Get-Content (Join-Path $root 'package.json') | ConvertFrom-Json).version). Open a new terminal and run: nosh setup"
