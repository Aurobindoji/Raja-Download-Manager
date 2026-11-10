$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$protocolKey = "HKCU:\Software\Classes\raja"
if (Test-Path -LiteralPath $protocolKey) {
    $uiExecutable = Join-Path $PSScriptRoot "raja-ui.exe"
    $commandKey = Join-Path $protocolKey "shell\open\command"
    $registeredCommand = if (Test-Path -LiteralPath $commandKey) {
        (Get-Item -LiteralPath $commandKey).GetValue("")
    }
    if ($registeredCommand -and $registeredCommand -like "*$uiExecutable*") {
        Remove-Item -LiteralPath $protocolKey -Recurse -Force
        Write-Output "Raja browser link handling was removed for the current Windows user."
    } else {
        Write-Output "The raja: protocol now belongs to another application; it was left unchanged."
    }
} else {
    Write-Output "Raja browser link handling is not registered."
}
