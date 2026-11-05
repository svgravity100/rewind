<#
  Builds Rewind for release and packs it two ways:
    dist\Rewind-<version>-portable.zip  — Rewind.exe + "portable" marker (settings stay next to the exe)
    dist\Rewind-<version>-setup.exe     — installer (Inno Setup 6), per-user, no admin rights

  Used by CI (.github/workflows/build.yml) and works the same on a developer PC:
    powershell -ExecutionPolicy Bypass -File scripts\package.ps1
  Without Inno Setup installed only the portable zip is made (unless -RequireInstaller).
#>
param(
    [string]$Version = "",
    [switch]$SkipBuild,
    [switch]$RequireInstaller
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$app = Join-Path $root "app"
$dist = Join-Path $root "dist"

if (-not $Version) {
    $line = Select-String -Path (Join-Path $app "Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    $Version = $line.Matches[0].Groups[1].Value
}
Write-Host "Version $Version"

if (-not $SkipBuild) {
    Push-Location $app
    try {
        cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    } finally { Pop-Location }
}
$exe = Join-Path $app "target\release\rewind.exe"
if (-not (Test-Path $exe)) { throw "No $exe — build first" }

if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force $dist | Out-Null

# ---- portable zip
$stage = Join-Path $dist "Rewind"
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exe (Join-Path $stage "Rewind.exe")
Set-Content -Encoding UTF8 (Join-Path $stage "portable") "Settings are kept in this folder (settings.json). Delete this file to keep them in %APPDATA% instead."
Set-Content -Encoding UTF8 (Join-Path $stage "README.txt") @"
Rewind $Version — портативная версия

Запуск: Rewind.exe. Установка не нужна.
Настройки хранятся в этой же папке (settings.json), потому что рядом лежит файл "portable".
Удалите файл "portable", чтобы настройки хранились в %APPDATA%\ReplayBuffer.
Клипы по умолчанию сохраняются в папку Видео\Rewind (меняется в настройках).
"@
$zip = Join-Path $dist "Rewind-$Version-portable.zip"
Compress-Archive -Path $stage -DestinationPath $zip
Remove-Item -Recurse -Force $stage
Write-Host "Portable: $zip"

# ---- installer
$iscc = (Get-Command iscc -ErrorAction SilentlyContinue).Source
if (-not $iscc) {
    $iscc = @("${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe", "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe") |
        Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $iscc) {
    if ($RequireInstaller) { throw "Inno Setup 6 (ISCC.exe) not found" }
    Write-Warning "Inno Setup 6 not found — installer skipped"
    exit 0
}
# The app icon is drawn by build.rs; take the .ico it wrote.
$ico = Get-ChildItem (Join-Path $app "target\release\build") -Recurse -Filter app.ico | Sort-Object LastWriteTime | Select-Object -Last 1
if (-not $ico) { throw "app.ico not found in the build output" }
& $iscc "/DAppVersion=$Version" "/DSourceExe=$exe" "/DOutputDir=$dist" "/DIconFile=$($ico.FullName)" (Join-Path $root "installer\rewind.iss")
if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }
Write-Host "Installer: $(Join-Path $dist "Rewind-$Version-setup.exe")"
