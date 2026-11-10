[CmdletBinding()]
param(
    [switch]$Force
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$uiExecutable = Join-Path $PSScriptRoot "raja-ui.exe"
if (-not (Test-Path -LiteralPath $uiExecutable -PathType Leaf)) {
    throw "raja-ui.exe must be in the same folder as this installer script."
}

$protocolKey = "HKCU:\Software\Classes\raja"
$commandKey = Join-Path $protocolKey "shell\open\command"
if (Test-Path -LiteralPath $commandKey) {
    $existingCommand = (Get-Item -LiteralPath $commandKey).GetValue("")
    if ($existingCommand -and $existingCommand -notlike "*$uiExecutable*" -and -not $Force) {
        throw "The raja: protocol is already registered to another application: $existingCommand"
    }
}
New-Item -Path $commandKey -Force | Out-Null
Set-Item -Path $protocolKey -Value "URL:Raja download link"
New-ItemProperty -Path $protocolKey -Name "URL Protocol" -PropertyType String -Value "" -Force |
    Out-Null
$command = '"' + $uiExecutable + '" "%1"'
Set-Item -Path $commandKey -Value $command
Write-Output "Raja browser link handling is registered for the current Windows user."
Write-Output "Load the browser-extension folder as an unpacked Chrome or Edge extension."
