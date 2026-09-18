# install-task.ps1 — 一键安装：15 分钟定时采集 + 托盘开机自启
param([switch]$Uninstall)

$TaskName = "DevinUsageCollect"
$Py = (Get-Command python -ErrorAction SilentlyContinue).Source
if (-not $Py) { $Py = "python" }
$Script = Join-Path $PSScriptRoot "devin_usage.py"
$TrayExe = Join-Path $PSScriptRoot "devin-usage-tray\target\release\devin-usage-tray.exe"
$Shortcut = Join-Path ([Environment]::GetFolderPath("Startup")) "devin-usage-tray.lnk"

if ($Uninstall) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    Remove-Item $Shortcut -Force -ErrorAction SilentlyContinue
    Write-Output "已卸载 $TaskName 与托盘自启"; exit 0
}

# 定时采集（每 15 分钟）
$action = New-ScheduledTaskAction -Execute $Py -Argument "`"$Script`" collect" -WorkingDirectory $PSScriptRoot
$trigger = New-ScheduledTaskTrigger -Once -At (Get-Date) `
    -RepetitionInterval (New-TimeSpan -Minutes 15)
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -DontStopIfGoingOnBatteries `
    -AllowStartIfOnBatteries -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
    -Settings $settings -Description "Devin App/CLI/Cloud usage collector" -Force | Out-Null
Write-Output "已注册 $TaskName（每 15 分钟，python=$Py）"

# 托盘开机自启（Startup 快捷方式）
if (Test-Path $TrayExe) {
    $ws = New-Object -ComObject WScript.Shell
    $lnk = $ws.CreateShortcut($Shortcut)
    $lnk.TargetPath = $TrayExe
    $lnk.WorkingDirectory = $PSScriptRoot
    $lnk.Save()
    Write-Output "托盘已加入开机自启: $Shortcut"
} else {
    Write-Output "未找到托盘 exe，跳过自启（先 cargo build --release）"
}

Write-Output "手动验证: python `"$Script`" collect"
