//! Windows' own face detector, so white balance can go by skin.

use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::FaceAnalysis::FaceDetector;
use windows::Security::Cryptography::CryptographicBuffer;

use crate::worker::{lock, log};

const EVERY: Duration = Duration::from_millis(500);
const FRESH: Duration = Duration::from_secs(1);
/// Still finds a face across a room.
const WIDTH: usize = 480;

/// In frame pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Face {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

struct Grey {
    pixels: Vec<u8>,
    width: usize,
    height: usize,
    scale: usize,
}

type Found = Arc<Mutex<Option<(Instant, Face)>>>;

#[derive(Default)]
pub(crate) struct Finder {
    send: Option<SyncSender<Grey>>,
    found: Found,
    sent: Option<Instant>,
}

impl Finder {
    pub(crate) fn look(&mut self, pixels: &[u8], width: usize, rgb: bool) {
        if width == 0 || self.sent.is_some_and(|t| t.elapsed() < EVERY) {
            return;
        }
        self.sent = Some(Instant::now());
        let scale = width.div_ceil(WIDTH).max(1);
        let rows = pixels.chunks_exact(width * 3).step_by(scale);
        let height = rows.len();
        let mut grey = Vec::with_capacity(height * width.div_ceil(scale));
        for row in rows {
            grey.extend(
                row.as_chunks::<3>()
                    .0
                    .iter()
                    .step_by(scale)
                    .map(|&[r, g, b]| {
                        if rgb {
                            ((u32::from(r) * 77 + u32::from(g) * 150 + u32::from(b) * 29) >> 8)
                                as u8
                        } else {
                            r
                        }
                    }),
            );
        }
        let send = self
            .send
            .get_or_insert_with(|| start(Arc::clone(&self.found)));
        let frame = Grey {
            width: grey.len().checked_div(height).unwrap_or_default(),
            pixels: grey,
            height,
            scale,
        };
        // Full means the detector is busy, closed that it never started.
        send.try_send(frame).ok();
    }

    pub(crate) fn face(&self) -> Option<Face> {
        lock(&self.found)
            .filter(|(t, _)| t.elapsed() < FRESH)
            .map(|(_, f)| f)
    }
}

fn start(found: Found) -> SyncSender<Grey> {
    let (send, frames) = sync_channel::<Grey>(1);
    thread::Builder::new()
        .name("face".into())
        .spawn(move || {
            crate::mta();
            let detector = match FaceDetector::CreateAsync().and_then(|a| a.join()) {
                Ok(d) => d,
                Err(e) => {
                    return log(&format!(
                        "no face detector, so white balance goes by the room ({e})"
                    ));
                }
            };
            for frame in frames {
                match detect(&detector, &frame) {
                    Ok(Some(f)) => *lock(&found) = Some((Instant::now(), f)),
                    Ok(None) => {}
                    Err(e) => return log(&format!("the face detector stopped ({e})")),
                }
            }
        })
        .ok();
    send
}

fn detect(detector: &FaceDetector, frame: &Grey) -> windows_core::Result<Option<Face>> {
    let size = |n: usize| i32::try_from(n).unwrap_or_default();
    let buffer = CryptographicBuffer::CreateFromByteArray(&frame.pixels)?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Gray8,
        size(frame.width),
        size(frame.height),
    )?;
    let faces = detector.DetectFacesAsync(&bitmap)?.join()?;
    let biggest = faces
        .into_iter()
        .filter_map(|f| f.FaceBox().ok())
        .max_by_key(|b| b.Width * b.Height);
    Ok(biggest.map(|b| Face {
        x: b.X as usize * frame.scale,
        y: b.Y as usize * frame.scale,
        w: b.Width as usize * frame.scale,
        h: b.Height as usize * frame.scale,
    }))
}
