#Requires -RunAsAdministrator
# Installs Viewfinder from a release zip or the repository's target\release, or removes it
# with -Uninstall.
# Frame Server runs as LOCAL SERVICE and can't read a user profile, so the
# DLL has to live under Program Files and be registered machine-wide.
# -User is who asked. An administrator's password at the prompt runs this as that
# administrator, and the login start, shortcuts and tasks belong to the person who asked.
param([switch]$Uninstall, [string]$User = "$env:USERDOMAIN\$env:USERNAME")

$ErrorActionPreference = 'Stop'
# Setup starts this in its own window, which would close before the error could be read.
trap { Write-Host $_ -ForegroundColor Red; Read-Host 'Press Enter to close'; break }
$clsid = '{D6F48B83-FB1F-4421-BE38-B81B93E940ED}'
# Not plain Viewfinder, since Discord takes viewfinder\viewfinder.exe for the puzzle game of that
# name and pops its overlay up.
$dest = Join-Path $env:ProgramFiles 'Viewfinder Webcam'
$old = Join-Path $env:ProgramFiles 'Viewfinder'
$key = "HKLM:\SOFTWARE\Classes\CLSID\$clsid"
# The camera DLL reads this to let only this user at the frames.
$owner = 'HKLM:\SOFTWARE\Viewfinder'
$sid = ([Security.Principal.NTAccount]$User).Translate([Security.Principal.SecurityIdentifier]).Value
# Their registry is only loaded while they're signed in. Otherwise the login start is left to
# Start with Windows in the app, and the desktop is found from their profile folder. An account
# that has never signed in has no profile yet, so it gets no desktop shortcut.
$hive = "Registry::HKEY_USERS\$sid"
$signedIn = Test-Path $hive
$userDir = (Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\$sid" -ErrorAction SilentlyContinue).ProfileImagePath
$desktop = if ($signedIn) { (Get-ItemProperty "$hive\Software\Microsoft\Windows\CurrentVersion\Explorer\Shell Folders").Desktop } elseif ($userDir) { Join-Path $userDir 'Desktop' }
$run = "$hive\Software\Microsoft\Windows\CurrentVersion\Run"
$task = 'Viewfinder Camera Name'
$start = 'Viewfinder Start'
$shortcuts = @(Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Viewfinder.lnk')
# A desktop moved somewhere else leaves no folder at the profile path.
if ($desktop -and (Test-Path -LiteralPath $desktop)) { $shortcuts += Join-Path $desktop 'Viewfinder.lnk' }

# Beside this script in a release zip, or in the build folder of a checkout.
$built = if (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'viewfinder.exe')) { $PSScriptRoot } else { Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release' }
$ours = @($dest, $old, $built) | ForEach-Object { Join-Path $_ 'viewfinder.exe' }

# Quit it the way the tray does first. Killing it while the NVIDIA effects run upsets the driver.
# Only copies from these folders count, since other programs are called viewfinder.exe too, and
# this runs as administrator.
$running = Get-Process viewfinder -ErrorAction SilentlyContinue | Where-Object { $ours -contains $_.Path }
if ($running) {
    # The running copy's own exe, since an older one might not know --quit.
    Start-Process $running[0].Path -ArgumentList '--quit' -Wait
    $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    $running | Where-Object { -not $_.HasExited } | Stop-Process -Force
}
# The old DLL stays loaded in Frame Server until the service restarts,
# which also drops any camera an app has open right now.
Stop-Service FrameServer -Force -ErrorAction SilentlyContinue

if ($Uninstall) {
    if ($signedIn) { Remove-ItemProperty $run -Name Viewfinder -ErrorAction SilentlyContinue }
    Remove-Item -LiteralPath $shortcuts -ErrorAction SilentlyContinue
    foreach ($name in $task, $start) {
        Unregister-ScheduledTask -TaskName $name -Confirm:$false -ErrorAction SilentlyContinue
    }
    Remove-Item $key, $owner -Recurse -ErrorAction SilentlyContinue
    Remove-Item $dest, $old -Recurse -ErrorAction SilentlyContinue
    # NVIDIA's Video Effects, where Setup in the app downloads them.
    if ($userDir) { Remove-Item -LiteralPath (Join-Path $userDir 'AppData\Local\Viewfinder\vfx') -Recurse -ErrorAction SilentlyContinue }
    'Viewfinder is uninstalled.'
    return
}

New-Item -ItemType Directory -Force $dest | Out-Null
# The script goes in too, so Setup in the installed app can run it to repair the install.
# Run from there, the files are already in place.
if ($built -ne $dest) {
    Copy-Item -LiteralPath (Join-Path $built 'viewfinder.exe'), (Join-Path $built 'viewfinder_vcam.dll') $dest -Force
}
if ($PSScriptRoot -ne $dest) { Copy-Item -LiteralPath $PSCommandPath $dest -Force }
Get-ChildItem -LiteralPath $dest | Unblock-File
if (Test-Path $old) {
    $vfx = Join-Path $old 'vfx'
    if ((Test-Path $vfx) -and -not (Test-Path (Join-Path $dest 'vfx'))) { Move-Item $vfx $dest }
    Remove-Item $old -Recurse -Force
}
$exe = Join-Path $dest 'viewfinder.exe'
$icon = Join-Path $dest 'viewfinder.ico'
Start-Process $exe -ArgumentList '--write-icon', "`"$icon`"" -Wait

New-Item -Force "$key\InprocServer32" | Out-Null
Set-ItemProperty $key -Name '(default)' -Value 'Viewfinder'
Set-ItemProperty "$key\InprocServer32" -Name '(default)' -Value (Join-Path $dest 'viewfinder_vcam.dll')
Set-ItemProperty "$key\InprocServer32" -Name ThreadingModel -Value Both
New-Item -Force $owner | Out-Null
Set-ItemProperty $owner -Name User -Value $sid

if ($signedIn) { Set-ItemProperty $run -Name Viewfinder -Value "`"$exe`" --background" }

$shell = New-Object -ComObject WScript.Shell
foreach ($path in $shortcuts) {
    $link = $shell.CreateShortcut($path)
    $link.TargetPath = $exe
    $link.WorkingDirectory = $dest
    $link.IconLocation = "$icon,0"
    $link.Description = 'Canon camera as a webcam'
    $link.Save()
}
# Windows adds " (Windows Virtual Camera)" to the camera's name and only an administrator can
# take it off, so the app starts this task each time it makes the camera. The task runs only
# viewfinder.exe --name-camera from Program Files, which a normal user can't replace.
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
$action = New-ScheduledTaskAction -Execute $exe -Argument '--name-camera'
# Highest only lifts an administrator, so for someone else it runs as SYSTEM and they may only
# start it. SYSTEM can't see their settings, so the camera is plain Viewfinder without the model.
$self = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value -eq $sid
$principal = if ($self) {
    New-ScheduledTaskPrincipal -UserId $User -LogonType Interactive -RunLevel Highest
}
else {
    New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
}
Register-ScheduledTask -TaskName $task -Action $action -Principal $principal -Settings $settings -Force | Out-Null
if (-not $self) {
    $scheduler = New-Object -ComObject Schedule.Service
    $scheduler.Connect()
    $scheduler.GetFolder('\').GetTask($task).SetSecurityDescriptor("D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;GRGX;;;$sid)", 0)
}

# Opens it again as the person who asked and without administrator rights. Started from here
# it would run as administrator. Tasks start below normal priority unless told, and the camera
# would lose frames to games.
$action = New-ScheduledTaskAction -Execute $exe
$principal = New-ScheduledTaskPrincipal -UserId $User -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan) -Priority 4
Register-ScheduledTask -TaskName $start -Action $action -Principal $principal -Settings $settings -Force | Out-Null
if ($signedIn) {
    Start-ScheduledTask -TaskName $start
    $where = if ($shortcuts.Count -gt 1) { 'on the desktop and in Start' } else { 'in Start' }
    "Viewfinder is installed in $dest, $where, and starts at login."
}
else {
    "Viewfinder is installed in $dest. $User isn't signed in, so Start with Windows in its General settings turns on the login start."
}
