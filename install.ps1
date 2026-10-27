# Installs ccnap for this user: downloads the latest release and runs `ccnap setup`.
#   irm https://raw.githubusercontent.com/abdulhaseeb7603/ccnap/main/install.ps1 | iex
$ErrorActionPreference = 'Stop'
$repo = 'abdulhaseeb7603/ccnap'
$base = "https://github.com/$repo/releases/latest/download"
$dir = Join-Path $env:TEMP "ccnap-setup-$PID"
New-Item -ItemType Directory -Force $dir | Out-Null
try {
    Invoke-WebRequest "$base/ccnap-x86_64-pc-windows-msvc.exe" -OutFile "$dir\ccnap.exe" -UseBasicParsing
    # The tray icon; `ccnap setup` installs it from next to ccnap.
    Invoke-WebRequest "$base/ccnap-tray-x86_64-pc-windows-msvc.exe" -OutFile "$dir\ccnap-tray.exe" -UseBasicParsing
    & "$dir\ccnap.exe" setup
} finally {
    Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
}
