#Requires -RunAsAdministrator
# Installs Viewfinder from target\release, or removes it with -Uninstall.
# Frame Server runs as LOCAL SERVICE and can't read a user profile, so the
# DLL has to live under Program Files and be registered machine-wide.
param([switch]$Uninstall)

$ErrorActionPreference = 'Stop'
$clsid = '{D6F48B83-FB1F-4421-BE38-B81B93E940ED}'
$dest = Join-Path $env:ProgramFiles 'Viewfinder'
$key = "HKLM:\SOFTWARE\Classes\CLSID\$clsid"
$run = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$task = 'Viewfinder Camera Name'
$shortcuts = (Join-Path ([Environment]::GetFolderPath('Desktop')) 'Viewfinder.lnk'),
(Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Viewfinder.lnk')

# Quit it the way the tray does first. Killing it while the NVIDIA effects run upsets the driver.
$running = Get-Process viewfinder -ErrorAction SilentlyContinue
if ($running) {
    Start-Process $running[0].Path -ArgumentList '--quit' -Wait
    $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    Get-Process viewfinder -ErrorAction SilentlyContinue | Stop-Process -Force
}
# The old DLL stays loaded in Frame Server until the service restarts,
# which also drops any camera an app has open right now.
Stop-Service FrameServer -Force -ErrorAction SilentlyContinue

if ($Uninstall) {
    Remove-ItemProperty $run -Name Viewfinder -ErrorAction SilentlyContinue
    Remove-Item $shortcuts -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
    Remove-Item $key -Recurse -ErrorAction SilentlyContinue
    Remove-Item $dest -Recurse -ErrorAction SilentlyContinue
    'Viewfinder is uninstalled.'
    return
}

$built = Join-Path $PSScriptRoot 'target\release'
New-Item -ItemType Directory -Force $dest | Out-Null
Copy-Item (Join-Path $built 'viewfinder.exe'), (Join-Path $built 'viewfinder_vcam.dll') $dest -Force
Get-ChildItem $dest | Unblock-File
$exe = Join-Path $dest 'viewfinder.exe'
$icon = Join-Path $dest 'viewfinder.ico'
Start-Process $exe -ArgumentList '--write-icon', "`"$icon`"" -Wait

# NVIDIA's super resolution and frame generation DLLs come from their own wheel,
# checked against the hash on pypi.nvidia.com. Only seven of its files are needed.
$vfx = Join-Path $dest 'vfx'
$needed = 'cudart64_12.dll', 'NVCVImage.dll', 'nvngxruntime.dll', 'NVVideoEffects.dll', 'nvVFXVideoSuperRes.dll', 'nvngx_vsr.dll', 'nvVFXVideoFrameGeneration.dll'
if (($needed | Where-Object { -not (Test-Path (Join-Path $vfx $_)) }).Count -gt 0) {
    $wheel = Join-Path $env:TEMP 'nvidia_vfx-0.2.0.0-cp312-abi3-win_amd64.whl'
    $hash = 'CEBAA12411F7E22B39293205C5C4D07B8073D14CA1E0E7727EE4375207A36445'
    if (-not (Test-Path $wheel) -or (Get-FileHash $wheel).Hash -ne $hash) {
        'Downloading NVIDIA Video Effects (about 440 MB)...'
        Invoke-WebRequest 'https://pypi.nvidia.com/nvidia-vfx/nvidia_vfx-0.2.0.0-cp312-abi3-win_amd64.whl' -OutFile $wheel
    }
    if ((Get-FileHash $wheel).Hash -ne $hash) { throw "The NVIDIA download doesn't match its published hash." }
    New-Item -ItemType Directory -Force $vfx | Out-Null
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($wheel)
    try {
        foreach ($entry in $zip.Entries | Where-Object { $needed -contains $_.Name }) {
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $vfx $entry.Name), $true)
        }
    }
    finally { $zip.Dispose() }
    Remove-Item $wheel
}

New-Item -Force "$key\InprocServer32" | Out-Null
Set-ItemProperty $key -Name '(default)' -Value 'Viewfinder'
Set-ItemProperty "$key\InprocServer32" -Name '(default)' -Value (Join-Path $dest 'viewfinder_vcam.dll')
Set-ItemProperty "$key\InprocServer32" -Name ThreadingModel -Value Both

Set-ItemProperty $run -Name Viewfinder -Value "`"$exe`" --background"

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
$action = New-ScheduledTaskAction -Execute $exe -Argument '--name-camera'
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $task -Action $action -Principal $principal -Settings $settings -Force | Out-Null

"Viewfinder is installed in $dest, on the desktop and in Start, and starts at login."
