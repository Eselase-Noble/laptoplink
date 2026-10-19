# LaptopLink installer for Windows (PowerShell).
#
#   irm https://raw.githubusercontent.com/Eselase-Noble/laptoplink/production/scripts/install.ps1 | iex
#
# Downloads the latest prebuilt Windows release and installs the `laplink`
# (and `laptoplink`) commands into your user profile, then adds them to PATH.
# No admin rights needed.

$ErrorActionPreference = "Stop"
$repo   = "Eselase-Noble/laptoplink"
$target = "x86_64-pc-windows-msvc"

Write-Host ">> finding the latest release"
$headers = @{ "User-Agent" = "laplink-installer" }
$rel = Invoke-RestMethod -Headers $headers "https://api.github.com/repos/$repo/releases/latest"
$tag = $rel.tag_name
if (-not $tag) { throw "no published release found. See https://github.com/$repo/releases" }

$asset = "laplink-$tag-$target.zip"
$url    = "https://github.com/$repo/releases/download/$tag/$asset"
$dest   = Join-Path $env:LOCALAPPDATA "Programs\laplink"
$tmpZip = Join-Path $env:TEMP $asset
$tmpDir = Join-Path $env:TEMP "laplink-$tag-$target"

Write-Host ">> downloading $asset ($tag)"
Invoke-WebRequest -Headers $headers -Uri $url -OutFile $tmpZip

Write-Host ">> installing to $dest"
New-Item -ItemType Directory -Force -Path $dest | Out-Null
if (Test-Path $tmpDir) { Remove-Item -Recurse -Force $tmpDir }
Expand-Archive -Path $tmpZip -DestinationPath $env:TEMP -Force
Copy-Item (Join-Path $tmpDir "laplink.exe")    (Join-Path $dest "laplink.exe")    -Force
Copy-Item (Join-Path $tmpDir "laptoplink.exe") (Join-Path $dest "laptoplink.exe") -Force

# Add to the user PATH if it isn't there yet.
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$dest*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$dest", "User")
    Write-Host ">> added $dest to your PATH (open a NEW terminal to pick it up)"
}

Remove-Item -Force $tmpZip -ErrorAction SilentlyContinue
Write-Host ">> installed. In a new terminal, run:  laplink doctor"
