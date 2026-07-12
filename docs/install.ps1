# Diskhoji installer for Windows — https://diskhoji.org
#   irm https://diskhoji.org/install.ps1 | iex
$ErrorActionPreference = "Stop"

$repo = "singhpratech/diskhoji"
$rel = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
$asset = $rel.assets | Where-Object { $_.name -like "*windows-x86_64.zip" } | Select-Object -First 1
if (-not $asset) {
    Write-Error "diskhoji: no Windows asset found in the latest release yet"
}

$dest = "$env:LOCALAPPDATA\Programs\diskhoji"
New-Item -ItemType Directory -Force -Path $dest | Out-Null

$zip = Join-Path $env:TEMP "diskhoji.zip"
$unpack = Join-Path $env:TEMP "diskhoji-unpack"
Write-Host "`u{25A6} diskhoji - fetching $($asset.browser_download_url)"
Invoke-WebRequest $asset.browser_download_url -OutFile $zip
if (Test-Path $unpack) { Remove-Item -Recurse -Force $unpack }
Expand-Archive $zip -DestinationPath $unpack -Force

$exe = Get-ChildItem $unpack -Recurse -Filter diskhoji.exe | Select-Object -First 1
Copy-Item $exe.FullName (Join-Path $dest "diskhoji.exe") -Force
Remove-Item $zip -Force
Remove-Item -Recurse -Force $unpack

# user PATH
$path = [Environment]::GetEnvironmentVariable("Path", "User")
if ($path -notlike "*$dest*") {
    [Environment]::SetEnvironmentVariable("Path", "$path;$dest", "User")
}

# Start Menu shortcut
$ws = New-Object -ComObject WScript.Shell
$lnk = $ws.CreateShortcut("$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Diskhoji.lnk")
$lnk.TargetPath = Join-Path $dest "diskhoji.exe"
$lnk.IconLocation = (Join-Path $dest "diskhoji.exe") + ",0"
$lnk.WorkingDirectory = $dest
$lnk.Description = "Diskhoji - every byte, accounted for"
$lnk.Save()

Write-Host "OK - Diskhoji installed to $dest"
Write-Host "     run 'diskhoji' in a new terminal, or launch it from the Start Menu"
