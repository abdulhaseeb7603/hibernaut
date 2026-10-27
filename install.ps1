# Installs hibernaut for this user: downloads the latest release and runs `hibernaut setup`.
#   irm https://raw.githubusercontent.com/abdulhaseeb7603/hibernaut/main/install.ps1 | iex
$ErrorActionPreference = 'Stop'
$repo = 'abdulhaseeb7603/hibernaut'
$base = "https://github.com/$repo/releases/latest/download"
$dir = Join-Path $env:TEMP "hibernaut-setup-$PID"
New-Item -ItemType Directory -Force $dir | Out-Null
try {
    Invoke-WebRequest "$base/hibernaut-x86_64-pc-windows-msvc.exe" -OutFile "$dir\hibernaut.exe" -UseBasicParsing
    # The tray icon; `hibernaut setup` installs it from next to hibernaut.
    Invoke-WebRequest "$base/hibernaut-tray-x86_64-pc-windows-msvc.exe" -OutFile "$dir\hibernaut-tray.exe" -UseBasicParsing
    & "$dir\hibernaut.exe" setup
} finally {
    Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
}
