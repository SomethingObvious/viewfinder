//! Drives a Canon EOS body over USB through the stock Windows Portable Devices driver with
//! Canon's PTP vendor operations. No EDSDK, libusb or driver swap.

mod arrival;
mod ptp;
mod quirks;
pub mod values;
pub mod wpd;

use std::fmt;
use std::thread::sleep;
use std::time::{Duration, Instant};

pub use arrival::{arrivals, arrived, watch_arrivals};
use ptp::{
    Event, OP_AF_CANCEL, OP_DO_AF, OP_DRIVE_LENS, OP_GET_DEVICE_INFO, OP_GET_EVENT,
    OP_GET_VIEWFINDER_DATA, OP_KEEP_DEVICE_ON, OP_OPEN_SESSION, OP_REQUEST_OLC_INFO_GROUP,
    OP_SET_EVENT_MODE, OP_SET_PROP_EX, OP_SET_REMOTE_MODE, RC_CANON_NOT_READY, RC_DEVICE_BUSY,
    RC_OK, RC_OPERATION_NOT_SUPPORTED, RC_SESSION_NOT_OPEN, allowed_values, events,
    parse_device_info, response_name, set_prop_payload, viewfinder_jpeg,
};
pub use ptp::{Prop, jpeg_size};
use windows::Win32::Devices::PortableDevices::{E_WPD_DEVICE_IS_HUNG, E_WPD_DEVICE_NOT_OPEN};
use windows::Win32::Foundation::{
    E_ACCESSDENIED, ERROR_DEVICE_NOT_CONNECTED, ERROR_DEVICE_REMOVED, ERROR_GEN_FAILURE,
    ERROR_OPERATION_ABORTED,
};
use windows::Win32::System::Com::{
    APTTYPE, APTTYPE_MAINSTA, APTTYPE_STA, APTTYPEQUALIFIER, CO_MTA_USAGE_COOKIE,
    CoDecrementMTAUsage, CoGetApartmentType, CoIncrementMTAUsage,
};

const CANON_VID: &str = "vid_04a9";
const EOS_1100D_PID: &str = "pid_3217";
const EVF_TFT: u32 = 1;
const EVF_PC: u32 = 2;
/// A 6D announces `EVFOutputDevice` as 0x80000002 before 2, so the top bit is a transition flag
/// (FC.SDK, "Live view output is a bit mask").
const EVF_TRANSITION: u32 = 0x8000_0000;
const KEEP_ALIVE: Duration = Duration::from_secs(10);
/// Auto Lighting Optimizer's strength when it's off, in both low bytes.
const ALO_OFF: u32 = 0x0303;

/// The value that turns Auto Lighting Optimizer off, or None if it's off already or the
/// value isn't one this understands. Newer bodies repeat the strength in both low bytes
/// (0x0202), older ones like the T3 send it alone (2). Either way 0 is standard, 1 low,
/// 2 strong and 3 off.
fn alo_off_value(alo: u32, allowed: &[u32]) -> Option<u32> {
    let strength = alo & 0xFF;
    let single = alo <= 0xFF && allowed.iter().all(|&v| v <= 0xFF);
    let paired = (alo >> 8) & 0xFF == strength;
    if strength == ALO_OFF & 0xFF || !(single || paired) {
        return None;
    }
    let off = |v: u32| {
        if single {
            v == ALO_OFF & 0xFF
        } else {
            v & 0xFFFF == ALO_OFF
        }
    };
    Some(
        allowed
            .iter()
            .copied()
            .find(|&v| off(v))
            .unwrap_or(if single {
                ALO_OFF & 0xFF
            } else {
                (alo & !0xFFFF) | ALO_OFF
            }),
    )
}

const MAX_DRAIN: usize = 64;
/// Checking events costs a round trip, and doing it every frame cost the T3 about 2 fps.
/// Every 10th is still three times a second.
const DRAIN_EVERY: u32 = 10;

/// What went wrong talking to the camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No Canon camera is visible to Windows Portable Devices.
    NotFound,
    /// The camera went away. Drop this `Camera` and call `Camera::open` again.
    Disconnected,
    /// The camera answered an operation with a PTP response code other than OK.
    Ptp {
        /// The PTP operation code.
        op: u16,
        /// The response code the camera sent back.
        code: u16,
    },
    /// Windows failed an operation with this HRESULT.
    Wpd {
        /// The PTP operation code.
        op: u16,
        /// What Windows Portable Devices returned.
        hresult: i32,
    },
    /// The camera hasn't announced this property, so it can't be set.
    Unannounced(Prop),
    /// The value isn't in the camera's current allowed list for the property.
    NotAllowed {
        /// The property being set.
        prop: Prop,
        /// The value the camera wouldn't take.
        value: u32,
    },
    /// An argument was outside the range the operation accepts.
    OutOfRange,
    /// The calling thread is in a single-threaded COM apartment.
    WrongApartment,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "no Canon camera found"),
            Self::Disconnected => write!(f, "camera disconnected"),
            Self::Ptp { op, code } => {
                write!(
                    f,
                    "camera answered 0x{code:04X} {} to operation 0x{op:04X}",
                    response_name(*code)
                )
            }
            Self::Wpd { op, hresult } => {
                write!(
                    f,
                    "WPD failed operation 0x{op:04X} with HRESULT 0x{:08X}",
                    *hresult as u32
                )
            }
            Self::Unannounced(p) => {
                write!(f, "camera hasn't announced {p:?} (0x{:04X})", p.code())
            }
            Self::NotAllowed { prop, value } => {
                write!(
                    f,
                    "0x{value:X} isn't an allowed value for {prop:?} right now"
                )
            }
            Self::OutOfRange => write!(f, "argument out of range"),
            Self::WrongApartment => {
                write!(
                    f,
                    "Camera needs a thread outside any single-threaded COM apartment"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

/// Result with this crate's `Error`.
pub type Result<T> = std::result::Result<T, Error>;

fn wpd_err(op: u16) -> impl Fn(windows_core::Error) -> Error {
    move |e| {
        let hr = e.code();
        let gone = [
            ERROR_DEVICE_NOT_CONNECTED.to_hresult(),
            ERROR_DEVICE_REMOVED.to_hresult(),
            ERROR_GEN_FAILURE.to_hresult(),
            ERROR_OPERATION_ABORTED.to_hresult(),
            E_ACCESSDENIED,
            E_WPD_DEVICE_NOT_OPEN,
            E_WPD_DEVICE_IS_HUNG,
        ];
        if gone.contains(&hr) {
            Error::Disconnected
        } else {
            Error::Wpd { op, hresult: hr.0 }
        }
    }
}

/// Whether a WPD failure means the device went away.
#[must_use]
pub fn disconnected(e: &windows_core::Error) -> bool {
    wpd_err(0)(e.clone()) == Error::Disconnected
}

const fn check(op: u16, code: u16) -> Result<()> {
    if code == RC_OK {
        Ok(())
    } else {
        Err(Error::Ptp { op, code })
    }
}

/// What `Camera::frame` produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    /// A JPEG is in the caller's buffer.
    Jpeg,
    /// The camera has no frame yet, which is normal for a second or so after live view starts.
    NotReady,
}

/// Keeps the process MTA alive for as long as a `Camera` exists.
#[derive(Debug)]
struct Mta(CO_MTA_USAGE_COOKIE);

impl Mta {
    #[expect(unsafe_code, reason = "joining the MTA is a COM call")]
    fn enter() -> Result<Self> {
        // SAFETY: takes nothing, and Drop pairs it with CoDecrementMTAUsage.
        let mta = Self(unsafe { CoIncrementMTAUsage() }.map_err(wpd_err(0))?);
        let mut kind = APTTYPE::default();
        let mut qualifier = APTTYPEQUALIFIER::default();
        // SAFETY: both out slots are live locals.
        unsafe { CoGetApartmentType(&raw mut kind, &raw mut qualifier) }.map_err(wpd_err(0))?;
        if kind == APTTYPE_STA || kind == APTTYPE_MAINSTA {
            return Err(Error::WrongApartment);
        }
        Ok(mta)
    }
}

impl Drop for Mta {
    #[expect(unsafe_code, reason = "leaving the MTA is a COM call")]
    fn drop(&mut self) {
        // SAFETY: the cookie came from CoIncrementMTAUsage and is used once.
        unsafe { CoDecrementMTAUsage(self.0) }.ok();
    }
}

#[derive(Debug, Default)]
struct Entry {
    value: Option<u32>,
    allowed: Vec<u32>,
}

/// Picks the camera's `PnP` id, preferring the EOS 1100D over any other Canon.
fn pick(ids: &[String]) -> Option<&String> {
    let has = |id: &String, part: &str| id.to_ascii_lowercase().contains(part);
    ids.iter()
        .find(|id| has(id, CANON_VID) && has(id, EOS_1100D_PID))
        .or_else(|| ids.iter().find(|id| has(id, CANON_VID)))
}

/// Whether a Canon camera is on and plugged in, by WPD's device list.
#[must_use]
pub fn present() -> bool {
    wpd::device_ids().is_ok_and(|ids| pick(&ids).is_some())
}

/// A Canon EOS camera opened through Windows Portable Devices.
///
/// Every call must come from an MTA thread or one that hasn't initialized COM, since `open`
/// keeps the MTA alive.
#[derive(Debug)]
pub struct Camera {
    wpd: wpd::Wpd,
    pulls: u32,
    unnamed: std::collections::BTreeMap<u16, u32>,
    model: String,
    ops: Vec<u16>,
    props: Vec<Entry>,
    events: Vec<u8>,
    frame: Vec<u8>,
    last_ping: Instant,
    evf_rest: u32,
    /// The camera's own Auto Lighting Optimizer setting, while live view runs with it off.
    alo_rest: Option<u32>,
    /// The camera's own white balance and colour temperature, while the app holds its own.
    wb_rest: Option<(u32, Option<u32>)>,
    live: bool,
    closed: bool,
    _mta: Mta,
}

#[expect(
    unsafe_code,
    reason = "the WPD interface pointers aren't Send on their own"
)]
#[expect(
    clippy::non_send_fields_in_send_ty,
    reason = "the device comes from PortableDeviceFTM, so its pointers work on any MTA thread"
)]
// SAFETY: the device object is created from PortableDeviceFTM, which aggregates the free-threaded
// marshaller, so its interface pointers are valid on any MTA thread.
unsafe impl Send for Camera {}

impl Camera {
    /// Opens the first Canon on WPD in remote mode, with its property cache filled.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::NotFound`] when no Canon is plugged in, [`Error::WrongApartment`] on
    /// an STA thread, or with what the camera or Windows answered while setting up.
    pub fn open() -> Result<Self> {
        let mta = Mta::enter()?;
        let ids = wpd::device_ids().map_err(wpd_err(0))?;
        let id = pick(&ids).ok_or(Error::NotFound)?;
        Self::open_with(id, mta)
    }

    /// Opens the body with this WPD `PnP` id, as [`Camera::open`] does.
    ///
    /// # Errors
    ///
    /// As for [`Camera::open`].
    pub fn open_at(pnp_id: &str) -> Result<Self> {
        Self::open_with(pnp_id, Mta::enter()?)
    }

    fn open_with(id: &str, mta: Mta) -> Result<Self> {
        let wpd = wpd::Wpd::open(id).map_err(wpd_err(0))?;
        let mut camera = Self {
            wpd,
            pulls: 0,
            unnamed: std::collections::BTreeMap::new(),
            model: String::new(),
            ops: Vec::new(),
            props: Prop::ALL.iter().map(|_| Entry::default()).collect(),
            events: Vec::new(),
            frame: Vec::new(),
            last_ping: Instant::now(),
            evf_rest: EVF_TFT,
            alo_rest: None,
            wb_rest: None,
            live: false,
            closed: false,
            _mta: mta,
        };
        camera.read_device_info()?;
        // An EOS from before live view, or a PowerShot with the older remote protocol.
        if !camera.supports(OP_GET_VIEWFINDER_DATA) {
            camera.closed = true;
            return Err(Error::Ptp {
                op: OP_GET_VIEWFINDER_DATA,
                code: RC_OPERATION_NOT_SUPPORTED,
            });
        }
        camera.prepare()?;
        Ok(camera)
    }

    fn read_device_info(&mut self) -> Result<()> {
        let mut data = Vec::new();
        let code = self
            .wpd
            .read(OP_GET_DEVICE_INFO, &[], &mut data)
            .map_err(wpd_err(OP_GET_DEVICE_INFO));
        let info = match code {
            Err(Error::Disconnected) => return Err(Error::Disconnected),
            Ok(RC_OK) => parse_device_info(&data),
            _ => None,
        };
        if let Some(info) = info {
            self.model = info.model;
            self.ops = info.ops;
        }
        if self.model.is_empty() {
            self.model = "Canon EOS".into();
        }
        Ok(())
    }

    /// The EOS remote session setup from `camera_prepare_canon_eos_capture` in config.c.
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the camera thread, where waiting for the body is fine"
    )]
    fn prepare(&mut self) -> Result<()> {
        if let Some(mode) = quirks::remote_mode(&self.model, &self.ops) {
            let code = self.call(OP_SET_REMOTE_MODE, &[mode])?;
            if code == RC_SESSION_NOT_OPEN {
                // WPD normally opens the session itself. If it hasn't, open one and go again.
                self.call(OP_OPEN_SESSION, &[1])?;
                self.call_ok(OP_SET_REMOTE_MODE, &[mode])?;
            } else {
                check(OP_SET_REMOTE_MODE, code)?;
            }
        }
        self.call_ok(OP_SET_EVENT_MODE, &[1])?;
        if self.ops.contains(&OP_REQUEST_OLC_INFO_GROUP) {
            self.call(OP_REQUEST_OLC_INFO_GROUP, &[0x1fff])?;
        }
        self.drain()?;
        // "The new EOS occasionally returned an empty event set ... likely because we are too
        // fast", so gphoto retries 10 times 50 ms until EVFOutputDevice shows up.
        for _ in 0..10 {
            if self.get(Prop::EvfOutput).is_some() {
                break;
            }
            sleep(Duration::from_millis(50));
            self.drain()?;
        }
        if quirks::wants_output_8(&self.model, &self.ops) {
            self.call_ok(OP_SET_EVENT_MODE, &[2])?;
            self.write_prop(Prop::EvfOutput, 8)?;
            sleep(Duration::from_secs(1));
            self.drain()?;
        }
        Ok(())
    }

    fn supports(&self, op: u16) -> bool {
        self.ops.is_empty() || self.ops.contains(&op)
    }

    fn call(&self, op: u16, params: &[u32]) -> Result<u16> {
        self.wpd.call(op, params).map_err(wpd_err(op))
    }

    fn call_ok(&self, op: u16, params: &[u32]) -> Result<()> {
        check(op, self.call(op, params)?)
    }

    /// Calls `GetEvent` until the camera has nothing more to report.
    fn drain(&mut self) -> Result<()> {
        for _ in 0..MAX_DRAIN {
            let code = self
                .wpd
                .read(OP_GET_EVENT, &[], &mut self.events)
                .map_err(wpd_err(OP_GET_EVENT))?;
            check(OP_GET_EVENT, code)?;
            let mut n = 0;
            for event in events(&self.events) {
                n += 1;
                match event {
                    Event::Value(prop, v) => {
                        if let Some(entry) = self.props.get_mut(prop.index()) {
                            entry.value = Some(v);
                        }
                    }
                    Event::Allowed(prop, list) => {
                        if let Some(entry) = self.props.get_mut(prop.index()) {
                            entry.allowed.clear();
                            entry.allowed.extend(allowed_values(prop, list));
                        }
                    }
                    Event::Unnamed(code, v) => {
                        self.unnamed.insert(code, v);
                    }
                    Event::Other => {}
                }
            }
            if n == 0 {
                break;
            }
        }
        Ok(())
    }

    fn keep_alive(&mut self) -> Result<()> {
        if self.last_ping.elapsed() < KEEP_ALIVE || !self.supports(OP_KEEP_DEVICE_ON) {
            return Ok(());
        }
        self.last_ping = Instant::now();
        match self.call(OP_KEEP_DEVICE_ON, &[]) {
            Err(Error::Disconnected) => Err(Error::Disconnected),
            _ => Ok(()),
        }
    }

    /// Writes a property with `SetDevicePropValueEx`, skipping values it already holds, since
    /// some bodies answer that with `DeviceBusy` (FC.SDK, `CaptureDestination`).
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the camera thread, where waiting for the body is fine"
    )]
    fn write_prop(&mut self, prop: Prop, value: u32) -> Result<()> {
        if self.get(prop) == Some(value) {
            return Ok(());
        }
        let payload = set_prop_payload(prop, value);
        let mut tries = 0;
        loop {
            // An undrained event queue also makes writes answer DeviceBusy (FC.SDK README).
            self.drain()?;
            let code = self
                .wpd
                .write(OP_SET_PROP_EX, &[], &payload)
                .map_err(wpd_err(OP_SET_PROP_EX))?;
            tries += 1;
            if code == RC_DEVICE_BUSY && tries < 3 {
                sleep(Duration::from_millis(50));
                continue;
            }
            check(OP_SET_PROP_EX, code)?;
            if let Some(entry) = self.props.get_mut(prop.index()) {
                entry.value = Some(value);
            }
            return Ok(());
        }
    }

    /// Starts live view to the PC, as `camera_capture_preview` in library.c does.
    ///
    /// # Errors
    ///
    /// Fails when the camera never announced its live view output or refuses a write.
    pub fn start_live_view(&mut self) -> Result<()> {
        if self.get(Prop::EvfMode).is_some_and(|m| m != 1) {
            // "in movie mode we get busy, but can proceed"
            match self.write_prop(Prop::EvfMode, 1) {
                Ok(())
                | Err(Error::Ptp {
                    code: RC_DEVICE_BUSY,
                    ..
                }) => {}
                Err(e) => return Err(e),
            }
        }
        let out = self
            .get(Prop::EvfOutput)
            .ok_or(Error::Unannounced(Prop::EvfOutput))?
            & !EVF_TRANSITION;
        // A body already sending to the PC is left over from an earlier session, so stop falls
        // back to the rear screen.
        self.evf_rest = if out & EVF_PC != 0 { EVF_TFT } else { out };
        if out & !EVF_TFT == 0 {
            self.write_prop(Prop::EvfOutput, EVF_PC)?;
        }
        // Quick AF drops the mirror and blanks the stream, so live view uses contrast AF.
        if self.get(Prop::AfMethod) == Some(0)
            && self.allowed(Prop::AfMethod).contains(&1)
            && self.write_prop(Prop::AfMethod, 1) == Err(Error::Disconnected)
        {
            return Err(Error::Disconnected);
        }
        self.alo_off()?;
        self.live = true;
        self.keep_alive()
    }

    /// Turns Auto Lighting Optimizer off for live view, since it shifts brightness with the
    /// scene even in M. A body that refuses is left as it is.
    fn alo_off(&mut self) -> Result<()> {
        let Some(alo) = self.get(Prop::Alo) else {
            return Ok(());
        };
        let Some(off) = alo_off_value(alo, self.allowed(Prop::Alo)) else {
            return Ok(());
        };
        match self.write_prop(Prop::Alo, off) {
            Ok(()) => {
                self.alo_rest = Some(alo);
                Ok(())
            }
            Err(Error::Disconnected) => Err(Error::Disconnected),
            Err(_) => Ok(()),
        }
    }

    /// Pulls one live view frame into `jpeg`, replacing its contents.
    ///
    /// # Errors
    ///
    /// Fails when the camera is gone or answers with an error other than not ready or busy.
    pub fn frame(&mut self, jpeg: &mut Vec<u8>) -> Result<Frame> {
        self.keep_alive()?;
        if self.pulls.is_multiple_of(DRAIN_EVERY) {
            self.drain()?;
        }
        self.pulls = self.pulls.wrapping_add(1);
        // 0x00200000 is what gphoto sends for the EOS 1000D (ptp_canon_eos_get_viewfinder_image).
        let params = [0x0020_0000, 0, 0];
        let pull = self
            .wpd
            .viewfinder(&params, &mut self.frame)
            .map_err(wpd_err(OP_GET_VIEWFINDER_DATA))?;
        match pull {
            wpd::Pull::Data => Ok(
                viewfinder_jpeg(&self.frame).map_or(Frame::NotReady, |data| {
                    jpeg.clear();
                    jpeg.extend_from_slice(data);
                    Frame::Jpeg
                }),
            ),
            wpd::Pull::Empty | wpd::Pull::Code(RC_CANON_NOT_READY | RC_DEVICE_BUSY) => {
                Ok(Frame::NotReady)
            }
            wpd::Pull::Code(code) => Err(Error::Ptp {
                op: OP_GET_VIEWFINDER_DATA,
                code,
            }),
        }
    }

    /// Stops live view and puts the output back on the rear screen or off.
    ///
    /// # Errors
    ///
    /// Fails when the camera is gone or refuses a write.
    pub fn stop_live_view(&mut self) -> Result<()> {
        self.live = false;
        if self.supports(OP_AF_CANCEL) && self.call(OP_AF_CANCEL, &[]) == Err(Error::Disconnected) {
            return Err(Error::Disconnected);
        }
        if self.get(Prop::EvfOutput).is_some_and(|o| o & EVF_PC != 0) {
            self.write_prop(Prop::EvfOutput, self.evf_rest)?;
        }
        if self.get(Prop::EvfMode).is_some_and(|m| m != 0) {
            self.write_prop(Prop::EvfMode, 0)?;
        }
        if let Some(alo) = self.alo_rest.take() {
            self.write_prop(Prop::Alo, alo)?;
        }
        self.restore_white_balance()
    }

    /// Sets white balance to `wb`, and the colour temperature to `kelvin` if given. The camera's
    /// own setting is kept the first time, for `restore_white_balance`.
    ///
    /// # Errors
    ///
    /// Fails like [`Camera::set`].
    pub fn hold_white_balance(&mut self, wb: u32, kelvin: Option<u32>) -> Result<()> {
        if self.wb_rest.is_none()
            && let Some(own) = self.get(Prop::WhiteBalance)
        {
            self.wb_rest = Some((own, self.get(Prop::ColorTemp)));
        }
        self.set(Prop::WhiteBalance, wb)?;
        kelvin.map_or(Ok(()), |k| self.set(Prop::ColorTemp, k))
    }

    /// Puts back the white balance the camera had before `hold_white_balance`.
    ///
    /// # Errors
    ///
    /// Fails when the camera is gone or refuses a write.
    pub fn restore_white_balance(&mut self) -> Result<()> {
        let Some((wb, kelvin)) = self.wb_rest.take() else {
            return Ok(());
        };
        if let Some(k) = kelvin {
            self.write_prop(Prop::ColorTemp, k)?;
        }
        self.write_prop(Prop::WhiteBalance, wb)
    }

    /// Stops live view if it's running and hands the camera back to its own controls.
    ///
    /// # Errors
    ///
    /// Returns the first thing that went wrong. Every step is still tried unless the camera
    /// is gone.
    pub fn close(mut self) -> Result<()> {
        self.shutdown()
    }

    /// The reset from `camera_unprepare_canon_eos_capture` in config.c.
    #[expect(unsafe_code, reason = "closing the device is a COM call")]
    fn shutdown(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let mut first = if self.live {
            self.stop_live_view()
        } else {
            Ok(())
        };
        if first != Err(Error::Disconnected) && quirks::eos_m(&self.model, &self.ops) {
            let off = self.write_prop(Prop::EvfOutput, 0);
            first = first.and(off);
        }
        if first != Err(Error::Disconnected) {
            let drained = self.drain();
            first = first.and(drained);
        }
        // gphoto follows remote mode 0 with 1 and then closes the session. WPD keeps the session
        // open after this, so a 1 would leave the body in remote mode with nobody keeping it
        // awake, and a T3 left like that hangs when its auto power off comes round.
        for (op, p) in [(OP_SET_REMOTE_MODE, 0), (OP_SET_EVENT_MODE, 0)] {
            if first == Err(Error::Disconnected) {
                break;
            }
            let r = self.call_ok(op, &[p]);
            first = first.and(r);
        }
        // SAFETY: the device is open, and nothing uses it after `closed` is set.
        unsafe { self.wpd.device.Close() }.ok();
        first
    }

    /// The cached value of a property, if the camera has announced it.
    #[must_use]
    pub fn get(&self, prop: Prop) -> Option<u32> {
        self.props.get(prop.index()).and_then(|e| e.value)
    }

    /// The values the camera currently accepts for a property, empty if it sent no list.
    #[must_use]
    pub fn allowed(&self, prop: Prop) -> &[u32] {
        self.props.get(prop.index()).map_or(&[], |e| &e.allowed)
    }

    /// Sets a property, refusing values outside the camera's allowed list.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Unannounced`] or [`Error::NotAllowed`] before touching the camera,
    /// or with what the camera answered.
    pub fn set(&mut self, prop: Prop, value: u32) -> Result<()> {
        if self.get(prop).is_none() {
            return Err(Error::Unannounced(prop));
        }
        let allowed = self.allowed(prop);
        if value & !prop.mask() != 0 || (!allowed.is_empty() && !allowed.contains(&value)) {
            return Err(Error::NotAllowed { prop, value });
        }
        self.write_prop(prop, value)
    }

    /// Runs one autofocus pass with the current AF method.
    ///
    /// # Errors
    ///
    /// Fails with what the camera answered.
    pub fn autofocus(&mut self) -> Result<()> {
        self.call_ok(OP_DO_AF, &[])?;
        self.drain()
    }

    /// Moves focus by a step of -3..=3 in live view, negative being nearer. The lens switch has
    /// to be on AF, since the body only drives the motor then.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::OutOfRange`] outside -3..=3, or with what the camera answered.
    pub fn drive_lens(&mut self, step: i8) -> Result<()> {
        let param = match step {
            0 => return Ok(()),
            -3..=-1 => u32::from(step.unsigned_abs()),
            1..=3 => 0x8000 | step as u32,
            _ => return Err(Error::OutOfRange),
        };
        self.call_ok(OP_DRIVE_LENS, &[param])?;
        self.drain()
    }

    /// The raw battery level, see `values::battery_label`.
    #[must_use]
    pub fn battery(&self) -> Option<u32> {
        self.get(Prop::Battery)
    }

    /// The exposure mode from the mode dial, see `values::ae_mode_label`.
    #[must_use]
    pub fn mode(&self) -> Option<u32> {
        self.get(Prop::AeModeDial)
            .or_else(|| self.get(Prop::AeMode))
    }

    /// Properties the camera reported that have no `Prop`, by code.
    #[must_use]
    pub const fn unnamed(&self) -> &std::collections::BTreeMap<u16, u32> {
        &self.unnamed
    }

    /// The camera's model name from its `DeviceInfo`.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_core::HRESULT;

    #[test]
    fn lighting_optimizer_turns_off_in_either_form() {
        // The T3 sends the strength alone and was on strong.
        assert_eq!(alo_off_value(2, &[]), Some(3));
        assert_eq!(alo_off_value(2, &[0, 1, 2, 3]), Some(3));
        assert_eq!(alo_off_value(3, &[]), None);
        // Newer bodies repeat it, with bit 16 keeping it on in M.
        assert_eq!(
            alo_off_value(0x1_0202, &[0x1_0000, 0x1_0303]),
            Some(0x1_0303)
        );
        assert_eq!(alo_off_value(0x0000, &[0x0000, 0x0303]), Some(0x0303));
        assert_eq!(alo_off_value(0x1_0303, &[]), None);
        assert_eq!(alo_off_value(0x0102, &[]), None);
    }

    #[test]
    fn pick_prefers_the_1100d() {
        let ids: Vec<String> = [
            r"\\?\usb#vid_045e&pid_0001#1#{6ac27878-a6fa-4155-ba85-f98f491d4f33}",
            r"\\?\usb#vid_04a9&pid_3145#2#{6ac27878-a6fa-4155-ba85-f98f491d4f33}",
            r"\\?\USB#VID_04A9&PID_3217#3#{6ac27878-a6fa-4155-ba85-f98f491d4f33}",
        ]
        .map(String::from)
        .into();
        assert!(pick(&ids).unwrap().contains("3217"));
        assert!(pick(ids.get(..2).unwrap()).unwrap().contains("3145"));
        assert_eq!(pick(ids.get(..1).unwrap()), None);
    }

    #[test]
    fn errors_name_the_operation_and_code() {
        let e = Error::Ptp {
            op: 0x9110,
            code: 0x2019,
        };
        assert_eq!(
            e.to_string(),
            "camera answered 0x2019 DeviceBusy to operation 0x9110"
        );
        let e = wpd_err(0x9116)(ERROR_DEVICE_NOT_CONNECTED.to_hresult().into());
        assert_eq!(e, Error::Disconnected);
        let e = wpd_err(0x9116)(HRESULT(0x8000_4005_u32.cast_signed()).into());
        assert_eq!(
            e.to_string(),
            "WPD failed operation 0x9116 with HRESULT 0x80004005"
        );
    }
}
