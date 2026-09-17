# install-task.ps1 — 注册/卸载 devin-usage 定时采集任务（每 15 分钟）
param([switch]$Uninstall)

$TaskName = "DevinUsageCollect"
$Py = (Get-Command python -ErrorAction SilentlyContinue).Source
if (-not $Py) { $Py = "C:\Users\meltemi\scoop\apps\miniconda3\current\python.exe" }
$Script = Join-Path $PSScriptRoot "devin_usage.py"

if ($Uninstall) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    Write-Output "已卸载 $TaskName"; exit 0
}

$action = New-ScheduledTaskAction -Execute $Py -Argument "`"$Script`" collect" -WorkingDirectory $PSScriptRoot
$trigger = New-ScheduledTaskTrigger -Once -At (Get-Date) `
    -RepetitionInterval (New-TimeSpan -Minutes 15)
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -DontStopIfGoingOnBatteries `
    -AllowStartIfOnBatteries -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
    -Settings $settings -Description "Devin App/CLI/Cloud usage collector" -Force | Out-Null
Write-Output "已注册 $TaskName（每 15 分钟，python=$Py）"
Write-Output "手动验证: python `"$Script`" collect"
