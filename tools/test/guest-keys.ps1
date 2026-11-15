# Types into a VM's console and saves a screenshot afterwards: a way into a guest that has
# nothing installed for it (no ssh, no agent). Runs `hyperwin console` as SYSTEM through a
# one-off scheduled task, because a VM started by the hyperWIN service lets only SYSTEM open
# its video pipe. Run it on the host, from an elevated prompt.
#   guest-keys.ps1 -Vm w10 -Size 1280x800 -Seconds 30 -KeysFile C:\keys.txt -Out C:\shot.png
# The keys file is one line in `hyperwin console --type`'s syntax, for example
#   {win+r}{wait}powershell -NoExit -Command "Get-PnpDevice -Class Display -PresentOnly"\n
# ({wait} is a second; \n is Enter). Without -KeysFile it only takes the screenshot.
# The console takes one connection: do not run it while a client looks at the VM.
param(
    [Parameter(Mandatory)][string]$Vm,
    [string]$Size = '1280x800',
    [int]$Seconds = 15,
    [string]$KeysFile = '',
    [string]$Out = "$env:TEMP\hyperwin-$Vm.png",
    [string]$Hyperwin = 'C:\Program Files\hyperWIN\hyperwin.exe'
)
$ErrorActionPreference = 'Stop'
$log = [IO.Path]::ChangeExtension($Out, '.log')
Remove-Item $log, $Out -ErrorAction SilentlyContinue
$type = ''
if ($KeysFile) { $type = "--type-file `"$KeysFile`"" }
$bat = [IO.Path]::ChangeExtension($Out, '.cmd')
Set-Content -Encoding ascii $bat "@`"$Hyperwin`" console hyperwin-$Vm.BasicSession $Size --seconds $Seconds --shot `"$Out`" $type > `"$log`" 2>&1"
$action = New-ScheduledTaskAction -Execute 'cmd.exe' -Argument "/c `"$bat`""
Register-ScheduledTask -TaskName 'hyperwin-guest-keys' -Action $action -User 'SYSTEM' -RunLevel Highest -Force | Out-Null
Start-ScheduledTask -TaskName 'hyperwin-guest-keys'
for ($i = 0; $i -lt ($Seconds + 60) -and (Get-ScheduledTask -TaskName 'hyperwin-guest-keys').State -eq 'Running'; $i++) { Start-Sleep 1 }
Unregister-ScheduledTask -TaskName 'hyperwin-guest-keys' -Confirm:$false
Remove-Item $bat -ErrorAction SilentlyContinue
Get-Content $log -Tail 3
