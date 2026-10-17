param([switch]$RemoveData, [switch]$NoPath, [switch]$NoScheduledTask, [string]$BinRoot)
$bin = if ($BinRoot) { [IO.Path]::GetFullPath($BinRoot) } else { Join-Path $env:LOCALAPPDATA "NOSH\bin" }; $wrapper = Join-Path $bin "nosh.cmd"
if (-not $NoScheduledTask) { & schtasks.exe /Delete /F /TN "NOSH noshd" 2>$null | Out-Null }
if (Test-Path -LiteralPath $wrapper) { Remove-Item -LiteralPath $wrapper }
if (-not $NoPath) { $path = [Environment]::GetEnvironmentVariable("Path", "User"); [Environment]::SetEnvironmentVariable("Path", (($path -split ";" | Where-Object { $_ -and $_ -ne $bin }) -join ";"), "User") }
if ($RemoveData) { $data = Join-Path $env:LOCALAPPDATA "NOSH"; if (Test-Path -LiteralPath $data) { Remove-Item -LiteralPath $data -Recurse -Force } }
Write-Host "NOSH command removed. Project repositories and data remain unless -RemoveData was supplied."
