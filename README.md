# Viewfinder

Viewfinder turns a Canon EOS Rebel T3 (EOS 1100D) into a webcam on Windows 11, in place of Canon's EOS Webcam Utility, which doesn't support that body any more. It shows up in apps as "Viewfinder Windows Virtual Camera".

It talks to the camera with Canon's own PTP commands through the Windows camera driver that is already installed, so there's no Canon SDK and no driver swap. Live view only runs while an app is using the camera or the Viewfinder window is open, and the mirror comes back down 10 seconds after the last one goes away.

From the tray window you can set ISO, shutter, aperture, exposure compensation, white balance and picture style, run autofocus or nudge focus in six steps, and move the 16:9 crop up or down inside the camera's 3:2 frame.

## Parts

- `canon` drives the camera over WPD (session setup, live view, properties, focus).
- `app` is the tray app. It decodes each live view JPEG straight to YCbCr, converts BT.601 full range to BT.709 video range, sharpens and scales into the size the calling app asked for.
- `vsr` runs NVIDIA's VideoSuperRes on the GPU. It upscales the cropped live view to the size the app asked for and takes out JPEG blocking on the way, in about 7 ms a frame on an RTX 4070 Laptop. Without an NVIDIA GPU the CPU path does the scaling instead.
- `vcam` is the media source DLL that the Windows Frame Server loads. It copies the newest frame out of shared memory and shows dark grey when there's no feed.
- `shared` is the shared-memory layout both sides use.

## Build and Install

```powershell
cargo build --release
```

Then, from an administrator PowerShell in this folder:

```powershell
.\install.ps1
```

This copies both files to `C:\Program Files\Viewfinder`, registers the DLL and starts the app at login. The first run also downloads NVIDIA's Video Effects wheel from pypi.nvidia.com (about 440 MB, checked against its published hash) and keeps the six DLLs super resolution needs, since those are NVIDIA's to distribute and aren't in this repo. Run it again after every build. `.\install.ps1 -Uninstall` removes everything.

Quit EOS Webcam Utility and EOS Utility first, since only one program can hold a session with the camera at a time.

## Limits

The camera sends 1056x704 JPEG at roughly 15 to 25 fps over USB, so that's the ceiling on detail and smoothness, and the output is scaled up from it. Keep the mode dial on P, Tv, Av or M, because scene modes ignore exposure changes. For long calls, use the ACK-E10 mains adapter, as live view drains the battery in an hour or two.
