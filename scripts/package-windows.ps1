param(
    [string]$OutputDirectory = "target\package"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
$releaseDirectory = Join-Path $root "target\release"
$manifest = Get-Content (Join-Path $root "Cargo.toml") -Raw
if ($manifest -notmatch '(?m)^version = "([^"]+)"') {
    throw "Could not read the Raja package version from Cargo.toml."
}
$version = $Matches[1]

$resolvedOutputDirectory = if ([IO.Path]::IsPathRooted($OutputDirectory)) {
    $OutputDirectory
} else {
    Join-Path $root $OutputDirectory
}
New-Item -ItemType Directory -Path $resolvedOutputDirectory -Force | Out-Null

$stageDirectory = Join-Path $env:TEMP "Raja-package-$([guid]::NewGuid())"
$archivePath = Join-Path $resolvedOutputDirectory "Raja-$version-windows-x64.zip"

try {
    New-Item -ItemType Directory -Path $stageDirectory | Out-Null
    foreach ($name in @(
        "raja-ui.exe",
        "raja-core.exe",
        "raja-browser-host.exe",
        "README.md",
        "LICENSE",
        "Install-RajaBrowserIntegration.ps1",
        "Uninstall-RajaBrowserIntegration.ps1"
    )) {
        $source = if ($name.EndsWith(".exe")) {
            Join-Path $releaseDirectory $name
        } else {
            Join-Path $root $name
        }
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Required package file is missing: $source"
        }
        Copy-Item -LiteralPath $source -Destination $stageDirectory
    }
    $extensionSource = Join-Path $root "browser-extension"
    if (-not (Test-Path -LiteralPath $extensionSource -PathType Container)) {
        throw "Required browser extension folder is missing: $extensionSource"
    }
    Copy-Item -LiteralPath $extensionSource `
        -Destination (Join-Path $stageDirectory "browser-extension") -Recurse

    Compress-Archive -Path (Join-Path $stageDirectory "*") `
        -DestinationPath $archivePath -Force
    Write-Output "Created portable Windows package: $archivePath"
} finally {
    if (Test-Path -LiteralPath $stageDirectory) {
        Remove-Item -LiteralPath $stageDirectory -Recurse -Force
    }
}
