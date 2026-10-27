# Installs ccnap for this user: downloads the latest release and runs `ccnap setup`.
#   irm https://raw.githubusercontent.com/abdulhaseeb7603/ccnap/main/install.ps1 | iex
$ErrorActionPreference = 'Stop'
$repo = 'abdulhaseeb7603/ccnap'
$tmp = Join-Path $env:TEMP "ccnap-setup-$PID.exe"
try {
    Invoke-WebRequest "https://github.com/$repo/releases/latest/download/ccnap-x86_64-pc-windows-msvc.exe" -OutFile $tmp -UseBasicParsing
    & $tmp setup
} finally {
    Remove-Item $tmp -ErrorAction SilentlyContinue
}
