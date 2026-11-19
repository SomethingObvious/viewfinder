//! Viewfinder: a Canon EOS body as a Windows webcam, with a tray icon and a preview window.

#![windows_subsystem = "windows"]

mod exposure;
mod pipeline;
mod pixels;
mod ui;
mod vcam;
mod worker;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

use eframe::egui::{self, ViewportCommand};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows::core::w;

use pipeline::Look;
use worker::Hub;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    if let Some(done) = run_command(&args) {
        return done;
    }
    let background = args.iter().any(|a| a == "--background");
    // A second launch, from the desktop or Start, shows the copy already running in the
    // tray. Two copies would fight over the camera.
    let Some(raise) = first_instance() else {
        return Ok(());
    };
    let hub = Hub::new(load_look());
    *worker::lock(&hub.exposure) = exposure::load();
    let (commands, receiver) = mpsc::channel();
    worker::spawn(hub.clone(), receiver);
    let (stop_vcam, vcam_thread) = spawn_vcam(hub.clone());

    let quitting = Arc::new(AtomicBool::new(false));
    let menu = Menu::new();
    let open = MenuItem::new("Open Viewfinder", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append_items(&[&open, &quit]).ok();
    let tray = Icon::from_rgba(mark(32), 32, 32).ok().and_then(|icon| {
        TrayIconBuilder::new()
            .with_tooltip("Viewfinder")
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .ok()
    });

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
            .with_decorations(false),
        ..Default::default()
    };
    let q = quitting;
    let result = eframe::run_native(
        "Viewfinder",
        options,
        Box::new(move |cc| {
            listen(&cc.egui_ctx, &raise, &open, &quit, &q);
            Ok(Box::new(ui::App::new(cc, hub, commands, q, !background)))
        }),
    );

    drop(tray);
    stop_vcam.send(()).ok();
    if let Some(t) = vcam_thread {
        t.join().ok();
    }
    result
}

/// Handles the flags that do one job and exit, returning how it went if `args` held one.
#[expect(unsafe_code, reason = "signalling the running copy is a Win32 call")]
#[expect(
    clippy::print_stderr,
    reason = "--write-icon runs from a build script, which reads its failure from stderr"
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
        if let Err(e) = vcam::name_interfaces() {
            worker::log(&format!("couldn't rename the camera ({e})"));
        }
        return Some(Ok(()));
    }
    if args.iter().any(|a| a == "--demo") {
        return Some(ui::demo::run());
    }
    if args.iter().any(|a| a == "--quit") {
        // Through the tray's Quit, so the NVIDIA effects get torn down properly. Killing the
        // process while they ran left the driver logging GPU errors.
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

/// Runs the virtual camera on its own MTA thread until the sender is used or dropped.
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

/// Shows the window when a second launch or the tray asks, and closes it when `--quit` or the
/// tray's Quit does.
#[expect(unsafe_code, reason = "waiting on the named events is Win32")]
fn listen(
    ctx: &egui::Context,
    raise: &Raise,
    open: &MenuItem,
    quit: &MenuItem,
    q: &Arc<AtomicBool>,
) {
    let ctx = ctx.clone();
    let show = move |ctx: &egui::Context| {
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        ctx.request_repaint();
    };
    let c = ctx.clone();
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
        // SAFETY: the name is a static wide string.
        let Ok(event) = (unsafe { CreateEventW(None, false, false, w!("Local\\Viewfinder-quit")) })
        else {
            return;
        };
        // SAFETY: the event was just created and is never closed.
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
    let (open_id, quit_id, quitting) = (open.id().clone(), quit.id().clone(), q.clone());
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == open_id {
            show(&ctx);
        } else if e.id == quit_id {
            quitting.store(true, Ordering::Relaxed);
            ctx.send_viewport_cmd(ViewportCommand::Close);
            ctx.request_repaint();
        }
    }));
}

/// Joins the calling thread to the multithreaded COM apartment, which the camera and Media
/// Foundation need.
#[expect(unsafe_code, reason = "joining the MTA is a COM call")]
pub fn mta() {
    // SAFETY: no reserved pointer, and a thread already in the MTA just gets S_FALSE.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().ok() };
}

/// The auto-reset event a second launch sets, wrapped so the handle can cross threads.
struct Raise(isize);

/// None when another copy is already running, after asking it to show its window.
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
    Some(Raise(event.0 as isize))
}

/// The yellow ring on a dark rounded tile, drawn here so there's no image file to ship.
/// Edges are antialiased by distance, which holds up from the tray's 16 pixels to 256.
#[expect(
    clippy::suboptimal_flops,
    clippy::imprecise_flops,
    reason = "mul_add and hypot would change the antialiased edges, and the icon is drawn once"
)]
#[must_use]
pub(crate) fn mark(n: u32) -> Vec<u8> {
    let s = n as f32 / 32.0;
    let c = n as f32 / 2.0;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
            let d = (dx * dx + dy * dy).sqrt();
            let ring = (7.5 * s - d).max(d - 11.5 * s).min(d - 4.0 * s);
            let ink = (0.5 - ring).clamp(0.0, 1.0);
            // Rounded square, corner radius 7 of 32.
            let (qx, qy) = (dx.abs() - (16.0 - 7.0) * s, dy.abs() - (16.0 - 7.0) * s);
            let outside =
                (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - 7.0 * s;
            let tile = (0.5 - outside).clamp(0.0, 1.0);
            let mix = |bg: f32, fg: f32| (bg + (fg - bg) * ink).round() as u8;
            rgba.extend_from_slice(&[
                mix(22.0, 255.0),
                mix(22.0, 214.0),
                mix(24.0, 10.0),
                (tile * 255.0).round() as u8,
            ]);
        }
    }
    rgba
}

/// A Windows .ico of the mark at the sizes Explorer asks for, as uncompressed 32-bit DIBs.
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

fn look_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Viewfinder").join("look.txt"))
}

fn load_look() -> Look {
    let mut look = Look::default();
    let Some(text) = look_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return look;
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<f32>() else {
            continue;
        };
        match key.trim() {
            "framing" => look.framing = value.clamp(0.0, 1.0),
            "sharpen" => look.sharpen = value.clamp(0.0, 1.5),
            "vsr_quality" => look.vsr_quality = (value as u32).min(23),
            "vsr_strength" => look.vsr_strength = value.clamp(0.0, 1.0),
            "denoise" => look.denoise = value as u32,
            "fix_pixels" => look.fix_pixels = value != 0.0,
            "fit" => look.fit = value != 0.0,
            "smooth" => look.smooth = value != 0.0,
            _ => {}
        }
    }
    look
}

/// Saves the picture settings for the next launch.
pub fn save_look(look: Look) {
    let Some(path) = look_path() else { return };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let text = format!(
        "framing={}\nsharpen={}\nvsr_quality={}\nvsr_strength={}\ndenoise={}\nfix_pixels={}\nfit={}\nsmooth={}\n",
        look.framing,
        look.sharpen,
        look.vsr_quality,
        look.vsr_strength,
        look.denoise,
        u8::from(look.fix_pixels),
        u8::from(look.fit),
        u8::from(look.smooth)
    );
    std::fs::write(path, text).ok();
}
