//! Every way Viewfinder gets live frames from a camera, behind one [`Source`] trait.
//!
//! Canon EOS bodies come through the `canon` crate, and Nikon and Sony bodies speak their own
//! PTP live view over the same WPD driver. Any other video capture device comes through Media
//! Foundation.

mod canon;
mod choice;
pub mod convert;
pub mod mf;
pub mod nikon;
pub mod ptp;
pub mod sony;
mod watch;
mod wpd;

use std::fmt;

pub use choice::{Choice, Vendor};
pub use watch::changes;
pub use wpd::{list, open, ptp_id};

use windows::Win32::System::Com::{CO_MTA_USAGE_COOKIE, CoDecrementMTAUsage, CoIncrementMTAUsage};

/// How a raw frame's pixels are laid out. Rows are packed, with no padding between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pixels {
    /// A full-size Y plane, then interleaved U and V at half size both ways.
    Nv12,
    /// Y0 U Y1 V for each pair of pixels.
    Yuy2,
}

/// What kind of frame a [`Source`] put in the caller's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A baseline JPEG, which carries its own size. A UVC MJPG frame may leave out its Huffman
    /// tables, and the standard ones apply.
    Jpeg,
    /// Uncompressed limited-range YCbCr.
    Raw {
        /// The pixel layout.
        pixels: Pixels,
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// BT.709 colour, else BT.601.
        bt709: bool,
    },
}

/// What went wrong with a camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No camera this backend handles is plugged in and on.
    NotFound,
    /// The camera went away. Drop the source and open it again.
    Disconnected,
    /// The camera is there but can't stream live view, and the reason says why.
    Unsupported(String),
    /// The camera or Windows refused something.
    Failed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "no camera found"),
            Self::Disconnected => write!(f, "camera disconnected"),
            Self::Unsupported(why) | Self::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {}

/// Result with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// One camera, open and ready to stream. Every call has to come from outside an STA.
pub trait Source: Send {
    /// The model name to show and to name the virtual camera after.
    fn model(&self) -> &str;

    /// Turns live view on. Safe to call when it already is.
    ///
    /// # Errors
    ///
    /// Fails when the camera refuses or is gone.
    fn start(&mut self) -> Result<()>;

    /// Pulls the next frame into `out`, replacing what was there, or `None` if none is ready.
    ///
    /// # Errors
    ///
    /// Fails when the camera is gone or answers with something other than not ready.
    fn frame(&mut self, out: &mut Vec<u8>) -> Result<Option<Format>>;

    /// Turns live view off and hands the camera back to its own controls.
    ///
    /// # Errors
    ///
    /// Returns the first thing that went wrong, though every step is still tried.
    fn close(self: Box<Self>) -> Result<()>;
}

/// Keeps the process MTA alive while a source holds COM objects.
#[derive(Debug)]
pub struct Mta(CO_MTA_USAGE_COOKIE);

#[expect(unsafe_code, reason = "the usage cookie is a raw pointer")]
// SAFETY: the cookie is an opaque token that CoDecrementMTAUsage takes back on any thread.
unsafe impl Send for Mta {}

impl Mta {
    /// Joins the MTA for as long as the returned value lives.
    ///
    /// # Errors
    ///
    /// Fails when COM won't start.
    #[expect(unsafe_code, reason = "joining the MTA is a COM call")]
    pub fn enter() -> Result<Self> {
        // SAFETY: takes nothing, and Drop pairs it with CoDecrementMTAUsage.
        unsafe { CoIncrementMTAUsage() }
            .map(Self)
            .map_err(|e| Error::Failed(format!("COM didn't start ({e})")))
    }
}

impl Drop for Mta {
    #[expect(unsafe_code, reason = "leaving the MTA is a COM call")]
    fn drop(&mut self) {
        // SAFETY: the cookie came from CoIncrementMTAUsage and is used once.
        unsafe { CoDecrementMTAUsage(self.0) }.ok();
    }
}
