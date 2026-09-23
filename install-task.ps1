# install-task.ps1 — 一键安装：15 分钟定时采集 + 托盘开机自启
# 采集器是 Rust 二进制 devin-usage-tray.exe（托盘进程本身也每 15min 自采一轮，
# 计划任务是冗余兜底）；不再需要 python。
param([switch]$Uninstall)

$TaskName = "DevinUsageCollect"
$TrayExe = Join-Path $PSScriptRoot "devin-usage-tray\target\release\devin-usage-tray.exe"
$Shortcut = Join-Path ([Environment]::GetFolderPath("Startup")) "devin-usage-tray.lnk"

if ($Uninstall) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    Remove-Item $Shortcut -Force -ErrorAction SilentlyContinue
    Write-Output "已卸载 $TaskName 与托盘自启"; exit 0
}

# 定时采集（每 15 分钟，Rust 采集器）
$action = New-ScheduledTaskAction -Execute $TrayExe -Argument "collect" -WorkingDirectory $PSScriptRoot
$trigger = New-ScheduledTaskTrigger -Once -At (Get-Date) `
    -RepetitionInterval (New-TimeSpan -Minutes 15)
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -DontStopIfGoingOnBatteries `
    -AllowStartIfOnBatteries -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
    -Settings $settings -Description "Devin/AI 工具用量采集（Rust）" -Force | Out-Null
Write-Output "已注册 $TaskName（每 15 分钟，$TrayExe collect）"

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

Write-Output "手动验证: `"$TrayExe`" collect"
