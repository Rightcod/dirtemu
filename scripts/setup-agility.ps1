param(
    [string]$Destination = "$PSScriptRoot\..\target\release",
    [string]$Cache = "$PSScriptRoot\..\..\ztrx-runtime\agility",
    [string]$Package = ""
)
$ScriptAuthor = "carunodev"
$ErrorActionPreference = "Stop"
$Version = "1.620.2"
$ExpectedHash = "70489669C2BBEAF318B62D72B0AC3E753D521387BFC848C9763791C4E920BFCC"
$Repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$Cache = [IO.Path]::GetFullPath($Cache)
if ($Cache.Equals($Repo, [StringComparison]::OrdinalIgnoreCase) -or $Cache.StartsWith($Repo + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw "The SDK download cache must be outside the repository."
}
New-Item -ItemType Directory -Force -Path $Cache | Out-Null
if (-not $Package) {
    $Package = Join-Path $Cache "microsoft.direct3d.d3d12.$Version.nupkg"
    if (-not (Test-Path -LiteralPath $Package)) {
        $Url = "https://api.nuget.org/v3-flatcontainer/microsoft.direct3d.d3d12/$Version/microsoft.direct3d.d3d12.$Version.nupkg"
        Invoke-WebRequest -Uri $Url -OutFile $Package -TimeoutSec 180
    }
}
$Package = (Resolve-Path -LiteralPath $Package).Path
if ((Get-FileHash -LiteralPath $Package -Algorithm SHA256).Hash -ne $ExpectedHash) {
    throw "The Direct3D SDK package hash does not match version $Version."
}
$Extracted = Join-Path $Cache $Version
Add-Type -AssemblyName System.IO.Compression.FileSystem
New-Item -ItemType Directory -Force -Path $Extracted | Out-Null
$Runtime = Join-Path ([IO.Path]::GetFullPath($Destination)) "D3D12"
New-Item -ItemType Directory -Force -Path $Runtime | Out-Null
$Archive = [IO.Compression.ZipFile]::OpenRead($Package)
try {
    foreach ($Name in "D3D12Core.dll", "d3d12SDKLayers.dll") {
        $Entry = $Archive.GetEntry("build/native/bin/x64/$Name")
        if ($null -eq $Entry) { throw "The SDK package is missing $Name" }
        $Source = Join-Path $Extracted $Name
        [IO.Compression.ZipFileExtensions]::ExtractToFile($Entry, $Source, $true)
        $Signature = Get-AuthenticodeSignature -LiteralPath $Source
        if ($Signature.Status -ne "Valid" -or $Signature.SignerCertificate.Subject -notmatch "Microsoft Corporation") {
            throw "Invalid Microsoft signature: $Name"
        }
        Copy-Item -LiteralPath $Source -Destination (Join-Path $Runtime $Name) -Force
    }
} finally {
    $Archive.Dispose()
}
Write-Host "Direct3D Agility $Version (SDK 620): $Runtime"
