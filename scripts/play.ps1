# ztrx launcher: runs the game for normal play (no scripted presses, no capture).
param(
    [Parameter(Mandatory = $true)][string]$Game,
    [string]$Cache = "$env:LOCALAPPDATA\ztrx\cache",
    [string]$Data = "$env:LOCALAPPDATA\ztrx\data",
    [string]$Exe = "$PSScriptRoot\..\target\release\victory.exe",
    [string]$Console = "series-s"
)
foreach ($v in "VICTORY_KEYS", "VICTORY_CAPTURE", "VICTORY_CAPTURE_FRAMES", "VICTORY_DUMP_TARGETS") {
    Remove-Item "Env:$v" -ErrorAction SilentlyContinue
}
foreach ($pair in @(
    @("VICTORY_SHADER_PROFILE", "auto"),
    @("VICTORY_MESH_FALLBACK", "on"),
    @("VICTORY_BVH_PROFILE", "auto"),
    @("VICTORY_MEMORY_PROFILE", "series-s"),
    @("VICTORY_RELAY", "2"),
    @("VICTORY_WIDE_BACK", "1"),
    @("VICTORY_JOIN_COPIES", "1"),
    @("VICTORY_CARD_BUFFERS_MIN_MB", "1")
)) {
    if (-not (Test-Path "Env:$($pair[0])")) {
        Set-Item "Env:$($pair[0])" $pair[1]
    }
}
New-Item -ItemType Directory -Force $Cache, $Data | Out-Null
$env:VICTORY_CACHE_DIR = $Cache
$env:VICTORY_DATA_DIR = $Data
& $Exe run $Game --console $Console --execute 2>&1 | Out-File "$env:TEMP\ztrx.log" -Encoding utf8
Write-Host "Exited with $LASTEXITCODE; log: $env:TEMP\ztrx.log"
