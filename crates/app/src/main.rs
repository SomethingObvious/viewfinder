//! Viewfinder: a Canon EOS body as a Windows webcam, with a tray icon and a preview window.

#![windows_subsystem = "windows"]

mod adjust;
mod autoframe;
mod exposure;
mod face;
mod focus;
mod general;
mod pipeline;
mod pixels;
mod selection;
mod setup;
mod store;
mod ui;
mod usage;
mod vcam;
mod white;
mod worker;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

use eframe::egui::{self, ViewportCommand};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};
use windows::core::w;

use pipeline::Look;
use worker::Hub;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    if let Some(done) = run_command(&args) {
        return done;
    }
    let background = args.iter().any(|a| a == "--background");
    setup::wait_after_restart(&args);
    // Two copies would fight over the camera, so a second launch shows the first.
    let Some(raise) = first_instance() else {
        return Ok(());
    };
    full_speed();
    let hub = Hub::new(load_look());
    *worker::lock(&hub.exposure) = exposure::load();
    *worker::lock(&hub.adjust) = adjust::load();
    *worker::lock(&hub.chosen) = selection::load();
    let prefs = general::load();
    *worker::lock(&hub.general) = prefs;
    let (commands, receiver) = mpsc::channel();
    worker::spawn(hub.clone(), receiver);
    usage::spawn(hub.clone());
    hotkeys(hub.clone(), commands.clone());
    let (stop_vcam, vcam_thread) = spawn_vcam(hub.clone());
    let exiting = hub.clone();

    let quitting = Arc::new(AtomicBool::new(false));
    let menu = Menu::new();
    let open = MenuItem::new("Open Viewfinder", true, None);
    let hide = MenuItem::new("Hide Camera", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append_items(&[&open, &hide, &quit]).ok();
    let tray: Option<TrayIcon> = Icon::from_rgba(mark(32), 32, 32).ok().and_then(|icon| {
        TrayIconBuilder::new()
            .with_tooltip("Viewfinder")
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .ok()
    });
    let tray_hwnd = tray.as_ref().map_or(0, |t| t.window_handle() as isize);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Viewfinder")
            .with_inner_size([1180.0, 640.0])
            .with_min_inner_size(ui::MIN_SIZE)
            .with_icon(egui::IconData {
                rgba: mark(64),
                width: 64,
                height: 64,
            })
            .with_visible(!background)
            .with_window_level(if prefs.on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            })
            .with_decorations(false),
        ..Default::default()
    };
    let result = eframe::run_native(
        "Viewfinder",
        options,
        Box::new(move |cc| {
            listen(&cc.egui_ctx, &raise, [&open, &hide, &quit], &quitting, &hub);
            let mut app = ui::App::new(cc, hub, commands, quitting, !background);
            app.tray = Some(Tray {
                hide,
                hwnd: tray_hwnd,
            });
            Ok(Box::new(app))
        }),
    );

    worker::let_go(&exiting);
    drop(tray);
    stop_vcam.send(()).ok();
    if let Some(t) = vcam_thread {
        t.join().ok();
    }
    result
}

#[expect(unsafe_code, reason = "signalling the running copy is a Win32 call")]
#[expect(
    clippy::print_stderr,
    reason = "--write-icon reports a failure on stderr"
)]
fn run_command(args: &[String]) -> Option<eframe::Result> {
    if let Some(i) = args.iter().position(|a| a == "--write-icon") {
        let path = args.get(i + 1).map_or("viewfinder.ico", String::as_str);
        if let Err(e) = std::fs::write(path, ico()) {
            eprintln!("Couldn't write {path} ({e})");
            std::process::exit(1);
        }
        return Some(Ok(()));
    }
    if args.iter().any(|a| a == "--name-camera") {
        // This runs as administrator, so it writes nothing in the user's folders, where a
        // junction could aim it at a protected file. Task Scheduler keeps the exit code.
        if let Err(e) = vcam::name_interfaces() {
            eprintln!("Couldn't rename the camera ({e})");
            std::process::exit(1);
        }
        return Some(Ok(()));
    }
    if args.iter().any(|a| a == "--remove-camera") {
        if let Err(e) = vcam::remove() {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return Some(Ok(()));
    }
    if args.iter().any(|a| a == "--demo") {
        return Some(ui::demo::run());
    }
    if args.iter().any(|a| a == "--quit") {
        // Killing the process while the NVIDIA effects ran left the driver logging GPU errors.
        // SAFETY: the name is a static wide string.
        let opened = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, w!("Local\\Viewfinder-quit")) };
        if let Ok(event) = opened {
            // SAFETY: the handle was just opened with EVENT_MODIFY_STATE.
            unsafe { SetEvent(event) }.ok();
        }
        return Some(Ok(()));
    }
    None
}

#[expect(unsafe_code, reason = "starting Media Foundation is a Win32 call")]
fn spawn_vcam(hub: Arc<Hub>) -> (mpsc::Sender<()>, Option<thread::JoinHandle<()>>) {
    let (stop_vcam, vcam_stopped) = mpsc::channel::<()>();
    let vcam_thread = thread::Builder::new()
        .name("vcam".into())
        .spawn(move || {
            mta();
            // SAFETY: takes constants only, and the thread is in the MTA.
            match unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
                .map_err(|e| format!("Media Foundation didn't start ({e})"))
                .and_then(|()| vcam::VirtualCamera::start())
            {
                Ok(camera) => {
                    vcam_stopped.recv().ok();
                    drop(camera);
                }
                Err(e) => hub.update(|s| s.vcam = Some(e)),
            }
        })
        .ok();
    (stop_vcam, vcam_thread)
}

pub(crate) struct Tray {
    pub(crate) hide: MenuItem,
    pub(crate) hwnd: isize,
}

impl Tray {
    #[expect(unsafe_code, reason = "tray notifications are a shell call")]
    pub(crate) fn notify(&self, title: &str, text: &str) {
        use windows::Win32::UI::Shell::{
            NIF_INFO, NIIF_RESPECT_QUIET_TIME, NIIF_WARNING, NIM_MODIFY, NOTIFYICONDATAW,
            Shell_NotifyIconW,
        };
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: windows::Win32::Foundation::HWND(self.hwnd as *mut std::ffi::c_void),
            uFlags: NIF_INFO,
            dwInfoFlags: NIIF_WARNING | NIIF_RESPECT_QUIET_TIME,
            ..Default::default()
        };
        for (to, from) in data
            .szInfoTitle
            .iter_mut()
            .zip(title.encode_utf16().take(63))
        {
            *to = from;
        }
        for (to, from) in data.szInfo.iter_mut().zip(text.encode_utf16().take(255)) {
            *to = from;
        }
        // The tray-icon crate doesn't say which id it gave its icon, so this tries the first few.
        // Read the id from tray-icon if it ever makes it public.
        for id in 1..=8 {
            data.uID = id;
            // SAFETY: `data` is a filled-in NOTIFYICONDATAW with its size set.
            if unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) }.as_bool() {
                return;
            }
        }
    }
}

#[expect(unsafe_code, reason = "waiting on the named events is Win32")]
fn listen(
    ctx: &egui::Context,
    raise: &Raise,
    [open, hide, quit]: [&MenuItem; 3],
    q: &Arc<AtomicBool>,
    hub: &Arc<Hub>,
) {
    let ctx = ctx.clone();
    let show = move |ctx: &egui::Context| {
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        ctx.request_repaint();
    };
    let c = ctx.clone();
    let quit_signal = raise.1;
    let raise = raise.0;
    thread::spawn(move || {
        // SAFETY: the event handle stays open for the life of the process.
        while unsafe { WaitForSingleObject(HANDLE(raise as *mut std::ffi::c_void), INFINITE) }.0
            == 0
        {
            show(&c);
        }
    });
    let (c, quitting) = (ctx.clone(), q.clone());
    thread::spawn(move || {
        let Some(event) = quit_signal.map(|q| HANDLE(q as *mut std::ffi::c_void)) else {
            return;
        };
        // SAFETY: `first_instance` made the event, and nothing closes it.
        if unsafe { WaitForSingleObject(event, INFINITE) }.0 == 0 {
            quitting.store(true, Ordering::Relaxed);
            c.send_viewport_cmd(ViewportCommand::Close);
            c.request_repaint();
        }
    });
    let c = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |e| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = e
        {
            show(&c);
        }
    }));
    let (open_id, hide_id, quit_id) = (open.id().clone(), hide.id().clone(), quit.id().clone());
    let (quitting, hub) = (q.clone(), hub.clone());
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == open_id {
            show(&ctx);
        } else if e.id == hide_id {
            hub.set_hidden(!hub.hidden());
        } else if e.id == quit_id {
            quitting.store(true, Ordering::Relaxed);
            ctx.send_viewport_cmd(ViewportCommand::Close);
            ctx.request_repaint();
        }
    }));
}

#[expect(unsafe_code, reason = "global hotkeys are Win32")]
fn hotkeys(hub: Arc<Hub>, commands: mpsc::Sender<worker::Command>) {
    let spawned = thread::Builder::new()
        .name("hotkeys".into())
        .spawn(move || {
            for key in *b"HF" {
                let keys = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
                // SAFETY: with no window the hotkey posts to this thread's queue.
                let taken = unsafe { RegisterHotKey(None, i32::from(key), keys, u32::from(key)) };
                if let Err(e) = taken {
                    let key = char::from(key);
                    worker::log(&format!("Ctrl+Alt+{key} is taken by another app ({e})"));
                }
            }
            let mut msg = MSG::default();
            // SAFETY: `msg` is a live MSG for GetMessageW to fill.
            while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.0 > 0 {
                if msg.message != WM_HOTKEY {
                    continue;
                }
                match u8::try_from(msg.wParam.0) {
                    Ok(b'H') => hub.set_hidden(!hub.hidden()),
                    Ok(b'F') => {
                        commands.send(worker::Command::Autofocus).ok();
                    }
                    _ => {}
                }
            }
        });
    if let Err(e) = spawned {
        worker::log(&format!("no hotkeys ({e})"));
    }
}

/// Joins the COM MTA, which the camera and Media Foundation need.
#[expect(unsafe_code, reason = "joining the MTA is a COM call")]
pub fn mta() {
    // SAFETY: no reserved pointer, and a thread already in the MTA just gets S_FALSE.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().ok() };
}

/// The raise event, and the quit event made before the window, so a `--quit` sent while it
/// starts isn't missed.
struct Raise(isize, Option<isize>);

/// The owner and SYSTEM can do anything, and administrators can set it. Without that an
/// administrator installing for someone else can't ask this copy to quit, so install.ps1 kills
/// it, which upsets the NVIDIA driver mid effect.
#[expect(
    unsafe_code,
    reason = "named events and security descriptors are Win32"
)]
fn quit_event() -> windows::core::Result<HANDLE> {
    use windows::Win32::Foundation::{CloseHandle, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL is a static wide string and `sd` is a live out slot, freed below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:(A;;GA;;;OW)(A;;GA;;;SY)(A;;0x100002;;;BA)"),
            SDDL_REVISION_1,
            &raw mut sd,
            None,
        )?;
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: `sa` outlives the call and the name is a static wide string.
    let event = unsafe {
        CreateEventW(
            Some(&raw const sa),
            false,
            false,
            w!("Local\\Viewfinder-quit"),
        )
    };
    // This copy is the first, so an event that's already there was made by something else,
    // which may have set its own permissions. It's left alone and the installer falls back to
    // ending the process.
    // SAFETY: takes nothing, and reads the error CreateEventW just left.
    let taken = event.is_ok() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    // SAFETY: the SDDL call says to free `sd` with LocalFree, and nothing uses it after this.
    unsafe { LocalFree(Some(HLOCAL(sd.0))) };
    if taken {
        if let Ok(e) = &event {
            // SAFETY: the handle was just opened and isn't used again.
            unsafe { CloseHandle(*e) }.ok();
        }
        return Err(ERROR_ALREADY_EXISTS.to_hresult().into());
    }
    event
}

/// Windows 11 can put a program with no window showing on efficiency clocks, and with the
/// window closed this still runs the camera, the focus and the call app's frames.
#[expect(
    unsafe_code,
    reason = "setting a process's power policy is a Win32 call"
)]
fn full_speed() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation,
    };
    // A control bit with its state bit clear turns that throttling off.
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    // SAFETY: `state` is a live local and the size passed is its own.
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    }
    .ok();
}

/// None when another copy is running, after asking it to show its window.
#[expect(unsafe_code, reason = "named events are Win32")]
fn first_instance() -> Option<Raise> {
    // SAFETY: the name is a static wide string.
    let event = unsafe { CreateEventW(None, false, false, w!("Local\\Viewfinder-raise")) }.ok()?;
    // SAFETY: takes nothing, and reads the error CreateEventW just left.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // SAFETY: the handle was just opened.
        unsafe { SetEvent(event) }.ok();
        return None;
    }
    Some(Raise(
        event.0 as isize,
        quit_event().ok().map(|q| q.0 as isize),
    ))
}

/// The small sizes are placed by hand on the pixel grid so they stay sharp.
pub(crate) struct Brackets {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    stroke: u32,
    arm: u32,
    leg: u32,
    pub(crate) tally: (u32, u32, u32),
}

impl Brackets {
    pub(crate) fn new(n: u32) -> Self {
        let at = |left, top, width, height, stroke, arm, leg, tally| Self {
            x: left,
            y: top,
            w: width,
            h: height,
            stroke,
            arm,
            leg,
            tally,
        };
        match n {
            16 => at(1, 4, 14, 8, 1, 4, 3, (10, 6, 2)),
            24 => at(3, 6, 18, 12, 2, 5, 4, (14, 9, 3)),
            32 => at(4, 8, 24, 16, 2, 7, 5, (19, 12, 4)),
            48 => at(7, 12, 34, 24, 4, 11, 8, (29, 18, 6)),
            _ => {
                let scale = |v: u32| (v * n).div_euclid(256).max(1);
                // The field takes the size's parity, so it sits square in the middle at any size.
                let width = scale(184) + (n - scale(184)) % 2;
                let height = scale(128) + (n - scale(128)) % 2;
                let (left, top) = ((n - width).div_euclid(2), (n - height).div_euclid(2));
                let side = scale(32);
                at(
                    left,
                    top,
                    width,
                    height,
                    scale(20),
                    scale(56),
                    scale(44),
                    (left + width - scale(36) - side, top + scale(32), side),
                )
            }
        }
    }

    pub(crate) const fn bars(&self) -> [[u32; 4]; 8] {
        let Self {
            x,
            y,
            w,
            h,
            stroke,
            arm,
            leg,
            ..
        } = *self;
        let (right, bottom) = (x + w, y + h);
        [
            [x, y, arm, stroke],
            [x, y, stroke, leg],
            [right - arm, y, arm, stroke],
            [right - stroke, y, stroke, leg],
            [x, bottom - stroke, arm, stroke],
            [x, bottom - leg, stroke, leg],
            [right - arm, bottom - stroke, arm, stroke],
            [right - stroke, bottom - leg, stroke, leg],
        ]
    }

    fn ink(&self, px: u32, py: u32) -> bool {
        self.bars()
            .iter()
            .any(|&[x, y, w, h]| (x..x + w).contains(&px) && (y..y + h).contains(&py))
    }

    fn tally(&self, px: u32, py: u32) -> bool {
        let (x, y, side) = self.tally;
        (x..x + side).contains(&px) && (y..y + side).contains(&py)
    }
}

#[must_use]
pub(crate) fn mark(n: u32) -> Vec<u8> {
    let b = Brackets::new(n);
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let colour = if b.tally(x, y) {
                [0xd8, 0x32, 0x2b]
            } else if b.ink(x, y) {
                [0xec, 0xeb, 0xe6]
            } else {
                [0x0a, 0x0a, 0x0a]
            };
            rgba.extend_from_slice(&colour);
            rgba.push(255);
        }
    }
    rgba
}

fn ico() -> Vec<u8> {
    const SIZES: [u32; 6] = [16, 24, 32, 48, 64, 256];
    let mut images = Vec::new();
    for n in SIZES {
        let mask = n.div_ceil(32) * 4 * n;
        let mut img = Vec::new();
        for v in [40u32, n, n * 2] {
            img.extend_from_slice(&v.to_le_bytes());
        }
        img.extend_from_slice(&1u16.to_le_bytes());
        img.extend_from_slice(&32u16.to_le_bytes());
        img.extend_from_slice(&0u32.to_le_bytes());
        img.extend_from_slice(&(n * n * 4 + mask).to_le_bytes());
        img.extend_from_slice(&[0; 16]);
        let rgba = mark(n);
        for row in rgba.chunks_exact(n as usize * 4).rev() {
            for px in row.as_chunks::<4>().0 {
                img.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        }
        img.resize(img.len() + mask as usize, 0);
        images.push((n, img));
    }
    let mut out = Vec::new();
    for v in [0u16, 1, SIZES.len() as u16] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let mut offset = 6 + 16 * SIZES.len() as u32;
    for (n, img) in &images {
        let side = if *n == 256 { 0 } else { *n as u8 };
        out.extend_from_slice(&[side, side, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for (_, img) in images {
        out.extend_from_slice(&img);
    }
    out
}

fn load_look() -> Look {
    let mut look = Look::default();
    let text = store::read("look.txt").unwrap_or_default();
    for (key, value) in store::numbers(&text) {
        match key {
            "framing" => look.framing = value.clamp(0.0, 1.0),
            "sharpen" => look.sharpen = value.clamp(0.0, 1.5),
            "vsr_quality" => look.vsr_quality = (value as u32).min(23),
            "vsr_strength" => look.vsr_strength = value.clamp(0.0, 1.0),
            // Anything outside NVIDIA's denoise levels is off, which is how the pipeline reads it.
            "denoise" => {
                look.denoise = Some(value as u32)
                    .filter(|d| (8..=11).contains(d))
                    .unwrap_or(0);
            }
            "fix_pixels" => look.fix_pixels = value != 0.0,
            "fit" => look.fit = value != 0.0,
            "smooth" => look.smooth = value != 0.0,
            "mirror" => look.mirror = value != 0.0,
            "upside_down" => look.upside_down = value != 0.0,
            "zoom" => look.zoom = (value as u32).clamp(100, 200),
            "pan" => look.pan = value.clamp(0.0, 1.0),
            "auto_frame" => look.auto_frame = value != 0.0,
            _ => {}
        }
    }
    look
}

/// Saves the picture settings.
pub fn save_look(look: Look) {
    let text = format!(
        "framing={}\nsharpen={}\nvsr_quality={}\nvsr_strength={}\ndenoise={}\nfix_pixels={}\nfit={}\nsmooth={}\nmirror={}\nupside_down={}\nzoom={}\npan={}\nauto_frame={}\n",
        look.framing,
        look.sharpen,
        look.vsr_quality,
        look.vsr_strength,
        look.denoise,
        u8::from(look.fix_pixels),
        u8::from(look.fit),
        u8::from(look.smooth),
        u8::from(look.mirror),
        u8::from(look.upside_down),
        look.zoom,
        look.pan,
        u8::from(look.auto_frame)
    );
    store::write("look.txt", &text);
}

#[cfg(test)]
mod tests {
    /// Rewrite it with `viewfinder.exe --write-icon crates/app/assets/viewfinder.ico`.
    #[test]
    fn built_in_icon_matches_the_mark() {
        assert!(
            super::ico() == include_bytes!("../assets/viewfinder.ico"),
            "crates/app/assets/viewfinder.ico is out of date"
        );
    }
}
