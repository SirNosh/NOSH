param([switch]$RemoveData)
$bin = Join-Path $env:LOCALAPPDATA "NOSH\bin"; $wrapper = Join-Path $bin "nosh.cmd"
& schtasks.exe /Delete /F /TN "NOSH noshd" 2>$null | Out-Null
if (Test-Path -LiteralPath $wrapper) { Remove-Item -LiteralPath $wrapper }
$path = [Environment]::GetEnvironmentVariable("Path", "User"); [Environment]::SetEnvironmentVariable("Path", (($path -split ";" | Where-Object { $_ -and $_ -ne $bin }) -join ";"), "User")
if ($RemoveData) { $data = Join-Path $env:LOCALAPPDATA "NOSH"; if ((Resolve-Path $data).Path -eq $data) { Remove-Item -LiteralPath $data -Recurse -Force } }
Write-Host "NOSH command removed. Project repositories and data remain unless -RemoveData was supplied."
