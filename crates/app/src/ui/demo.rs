//! `viewfinder.exe --demo`: the real window driven by a colour-bar card and a made-up
//! camera, so the layouts can be tried without the camera, the GPU or the saved settings.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use canon::Prop;
use eframe::egui;
use shared::{Format, NV12};

use super::App;
use crate::pipeline::Look;
use crate::usage;
use crate::worker::{Command, Hub, Setting, lock};

pub(crate) fn run() -> eframe::Result {
    let hub = hub();
    let (commands, receiver) = mpsc::channel();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Viewfinder Demo")
            .with_inner_size([1180.0, 640.0])
            .with_min_inner_size([760.0, 420.0])
            .with_icon(egui::IconData {
                rgba: crate::mark(64),
                width: 64,
                height: 64,
            })
            .with_decorations(false),
        ..Default::default()
    };
    eframe::run_native(
        "Viewfinder Demo",
        options,
        Box::new(move |cc| {
            let feed = hub.clone();
            std::thread::spawn(move || camera(&feed, &receiver));
            let mut app = App::new(cc, hub, commands, Arc::new(AtomicBool::new(true)), true);
            app.persist = false;
            app.cameras = None;
            Ok(Box::new(app))
        }),
    )
}

/// Stands in for the camera thread. The stage times change once a second with a spike now
/// and then, so the meter's peak can be seen holding and falling.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add would change rounding, and the made-up times must stay the same"
)]
fn camera(hub: &Hub, commands: &mpsc::Receiver<Command>) {
    let started = std::time::Instant::now();
    let mut second = 0;
    loop {
        match commands.recv_timeout(Duration::from_millis(40)) {
            Ok(Command::Set(prop, value)) => {
                let mut s = lock(&hub.status);
                if let Some(setting) = s.settings.iter_mut().find(|x| x.prop == Some(prop)) {
                    setting.value = Some(value);
                }
                drop(s);
                hub.repaint();
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if hub.compare.load(Ordering::Relaxed) || hub.want_original.swap(false, Ordering::Relaxed) {
            still(hub);
            hub.repaint();
        }
        let now = started.elapsed().as_secs();
        if now != second {
            second = now;
            let gpu = if now % 7 == 3 {
                16.8
            } else {
                7.4 + (now % 3) as f32 * 0.3
            };
            let mut s = lock(&hub.status);
            s.stage_ms = [5.1, 0.6, gpu, 1.6];
            s.frame_ms = s.stage_ms.iter().sum();
            drop(s);
            hub.repaint();
        }
    }
}

/// The camera's own picture as Preview's still, as the worker sends it.
pub(super) fn still(hub: &Hub) {
    let (w, h, rgba) = card(true);
    let mut p = lock(&hub.preview);
    (p.still_size, p.still, p.still_fresh) = ((w, h), rgba, true);
}

/// A made-up Rebel T3 with an app on the camera.
pub(crate) fn hub() -> Arc<Hub> {
    let hub = Hub::new(Look::default());
    let setting = |prop, value, allowed: &[u32]| Setting {
        prop: Some(prop),
        value: Some(value),
        allowed: allowed.to_vec(),
    };
    {
        let mut s = lock(&hub.status);
        s.camera = Some("Canon EOS REBEL T3".into());
        s.live = true;
        s.fps = 27.6;
        s.stage_ms = [5.1, 0.6, 7.4, 1.6];
        s.frame_ms = 14.7;
        s.consumer = Some(Format {
            width: 1920,
            height: 1080,
            fourcc: NV12,
        });
        s.battery = Some(2);
        s.mode = Some(3);
        s.vsr_available = true;
        s.stuck = 3;
        s.settings = vec![
            setting(Prop::Iso, 0x00, &[0x00, 0x48, 0x50, 0x58, 0x60, 0x68]),
            setting(
                Prop::Shutter,
                0x60,
                &[0x58, 0x5b, 0x5d, 0x60, 0x63, 0x65, 0x68],
            ),
            setting(
                Prop::Aperture,
                0x28,
                &[0x20, 0x23, 0x25, 0x28, 0x2b, 0x2d, 0x30],
            ),
            setting(
                Prop::ExposureComp,
                0x00,
                &[0xf8, 0xfb, 0xfd, 0x00, 0x03, 0x05, 0x08],
            ),
            setting(Prop::WhiteBalance, 0, &[0, 1, 2, 3, 4, 5, 8, 9]),
            setting(Prop::ColorTemp, 5200, &[]),
            setting(Prop::PictureStyle, 0x81, &[0x81, 0x82, 0x83]),
            setting(Prop::AfMethod, 1, &[0, 1, 2]),
        ];
    }
    let (w, h, rgba) = card(false);
    let mut p = lock(&hub.preview);
    let bins = (0..64u32)
        .map(|i| 400 + (i * 37 % 23) * 90 + if (20..44).contains(&i) { 2400 } else { 0 })
        .collect();
    (p.width, p.height, p.rgba, p.fresh, p.histogram) = (w, h, rgba, true, bins);
    drop(p);
    still(&hub);
    *lock(&usage::LATEST) = Some(usage::Usage {
        cpu: 23.0,
        cpu_own: 4.0,
        ram: 9_771_000_000,
        ram_total: 17_179_869_184,
        ram_own: 325_058_560,
        gpu: Some(usage::Gpu {
            busy: 31.0,
            own: 12.0,
            vram: 1_288_490_189,
        }),
    });
    hub
}

/// SMPTE-style bars over a ramp, standing in for the camera so no frame of a person is ever
/// shown. Black bars at the sides, as Fit produces. The "original" is flatter and grainy, the
/// way the camera's own picture looks next to the processed one.
fn card(original: bool) -> (usize, usize, Vec<u8>) {
    let (w, h) = (1280usize, 720usize);
    let inner = 1080usize;
    let left = (w - inner).div_euclid(2);
    let bars: [[u8; 3]; 7] = [
        [191, 191, 191],
        [191, 191, 0],
        [0, 191, 191],
        [0, 191, 0],
        [191, 0, 191],
        [191, 0, 0],
        [0, 0, 191],
    ];
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let px = if x < left || x >= left + inner {
                [0, 0, 0]
            } else if y < (h * 2).div_euclid(3) {
                bars.get(((x - left) * 7).div_euclid(inner))
                    .copied()
                    .unwrap_or_default()
            } else {
                let g = ((x - left) * 255).div_euclid(inner) as u8;
                [g, g, g]
            };
            let px = if original {
                let grain = i32::try_from((x * 7919 + y * 104_729) % 41).unwrap_or_default() - 20;
                px.map(|c| ((i32::from(c) * 3).div_euclid(4) + 24 + grain).clamp(0, 255) as u8)
            } else {
                px
            };
            rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    (w, h, rgba)
}
