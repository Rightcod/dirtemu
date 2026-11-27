# ztrx launcher: runs the game for normal play (no scripted presses, no capture).
param(
    [Parameter(Mandatory = $true)][string]$Game,
    [string]$Cache = "",
    [string]$Data = "",
    [string]$Exe = "$PSScriptRoot\..\target\release\victory.exe",
    [string]$Console = "series-x"
)
$ScriptAuthor = "carunodev"
$Repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..")).Path
$Runtime = "$Repo-runtime"
if (-not $Cache) { $Cache = Join-Path $Runtime "cache" }
if (-not $Data) { $Data = Join-Path $Runtime "data" }
foreach ($v in "VICTORY_KEYS", "VICTORY_CAPTURE", "VICTORY_CAPTURE_FRAMES", "VICTORY_DUMP_TARGETS", "VICTORY_DISABLE_UPSCALER", "VICTORY_STACKS") {
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
    @("VICTORY_CARD_BUFFERS_MIN_MB", "8"),
    @("VICTORY_CARD_BUFFERS_MB", "768"),
    @("VICTORY_CARD_WINDOWS_MB", "128"),
    @("VICTORY_CARD_REHASH_RUNS", "1"),
    @("VICTORY_TEXTURE_MB", "1536"),
    @("VICTORY_WINDOW_TAILS", "1"),
    @("VICTORY_PORT_CONSTANTS", "1")
)) {
    if (-not (Test-Path "Env:$($pair[0])")) {
        Set-Item "Env:$($pair[0])" $pair[1]
    }
}
$TempPath = Join-Path $Cache "temp"
New-Item -ItemType Directory -Force $Cache, $Data, $TempPath | Out-Null
$env:TEMP = (Resolve-Path -LiteralPath $TempPath).Path
$env:TMP = $env:TEMP
$env:VICTORY_CACHE_DIR = (Resolve-Path -LiteralPath $Cache).Path
$env:VICTORY_DATA_DIR = (Resolve-Path -LiteralPath $Data).Path
$LogPath = Join-Path $Cache "ztrx.log"
& $Exe run $Game --console $Console --execute --precompile 2>&1 | Tee-Object -FilePath $LogPath
Write-Host "Exited with $LASTEXITCODE; log: $LogPath"
