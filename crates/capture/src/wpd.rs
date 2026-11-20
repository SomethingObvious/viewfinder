//! PTP bodies through the `canon` crate's WPD transport, and the list of every camera.

use canon::wpd::{Wpd, device_ids, device_type, friendly_name};

use crate::nikon::Nikon;
use crate::ptp::Transport;
use crate::sony::Sony;
use crate::{Choice, Error, Mta, Result, Source, Vendor};

fn wpd_err(e: &windows_core::Error) -> Error {
    if canon::disconnected(e) {
        Error::Disconnected
    } else {
        Error::Failed(format!("WPD failed ({e})"))
    }
}

/// A WPD device, with the MTA kept alive for as long as it's open.
#[derive(Debug)]
struct Device {
    wpd: Wpd,
    _mta: Mta,
}

impl Transport for Device {
    fn call(&mut self, op: u16, params: &[u32]) -> Result<u16> {
        self.wpd.call(op, params).map_err(|e| wpd_err(&e))
    }

    fn read(&mut self, op: u16, params: &[u32], out: &mut Vec<u8>) -> Result<u16> {
        self.wpd.read(op, params, out).map_err(|e| wpd_err(&e))
    }

    fn write(&mut self, op: u16, params: &[u32], data: &[u8]) -> Result<u16> {
        self.wpd.write(op, params, data).map_err(|e| wpd_err(&e))
    }
}

/// `WPD_DEVICE_TYPE_CAMERA`.
const WPD_CAMERA: u32 = 1;

/// Every PTP body from a maker Viewfinder knows, with its WPD `PnP` id.
fn bodies() -> Result<Vec<(String, Choice)>> {
    let _mta = Mta::enter()?;
    let ids = device_ids().map_err(|e| wpd_err(&e))?;
    Ok(ids
        .into_iter()
        .filter_map(|id| {
            let vendor = Vendor::of(&id)?;
            // Sony phones share the cameras' USB vendor id, but their drivers say they're phones.
            if device_type(&id).is_some_and(|t| t != WPD_CAMERA) {
                return None;
            }
            let model = friendly_name(&id).ok()?;
            Some((id, Choice::Ptp { vendor, model }))
        })
        .collect())
}

/// The WPD `PnP` id of the PTP body `choice` names, if it's plugged in and on.
#[must_use]
pub fn ptp_id(choice: &Choice) -> Option<String> {
    let mut bodies = bodies().ok()?;
    let found: Vec<Choice> = bodies.iter().map(|b| b.1.clone()).collect();
    let at = choice.find(&found)?;
    Some(bodies.swap_remove(at).0)
}

/// Every camera Viewfinder could use: PTP bodies first, then webcams and capture cards. One
/// that Windows won't list is left out.
#[must_use]
pub fn list() -> Vec<Choice> {
    let mut all: Vec<Choice> = bodies()
        .unwrap_or_default()
        .into_iter()
        .map(|b| b.1)
        .collect();
    all.extend(crate::mf::choices().unwrap_or_default());
    all
}

/// Opens the camera `choice` names and nothing else.
///
/// # Errors
///
/// Fails with [`Error::NotFound`] when it isn't plugged in or is off, [`Error::Unsupported`]
/// when it has no live view over USB, or with what the camera answered.
pub fn open(choice: &Choice) -> Result<Box<dyn Source>> {
    let Choice::Ptp { vendor, model } = choice else {
        return Ok(Box::new(crate::mf::open_choice(choice)?));
    };
    if *vendor == Vendor::Canon && model.is_empty() {
        return Ok(Box::new(canon::Camera::open()?));
    }
    let mta = Mta::enter()?;
    let id = ptp_id(choice).ok_or(Error::NotFound)?;
    if *vendor == Vendor::Canon {
        return Ok(Box::new(canon::Camera::open_at(&id)?));
    }
    let device = Device {
        wpd: Wpd::open(&id).map_err(|e| wpd_err(&e))?,
        _mta: mta,
    };
    Ok(if *vendor == Vendor::Nikon {
        Box::new(Nikon::open(device)?)
    } else {
        Box::new(Sony::open(device)?)
    })
}
