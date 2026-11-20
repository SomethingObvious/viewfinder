//! Sony live view over PTP, for bodies with the USB connection set to PC Remote.
//!
//! It follows the SDIO handshake in `fixup_cached_deviceinfo` and the `PTP_VENDOR_SONY` case
//! of `camera_capture_preview` in gphoto's library.c.
//!
//! Untested on a real body. Every step and code is gphoto's.

use std::thread::sleep;
use std::time::Duration;

use crate::ptp::{
    DeviceInfo, OP_GET_DEVICE_INFO, OP_GET_OBJECT, OP_GET_OBJECT_INFO, RC_ACCESS_DENIED,
    RC_INVALID_OBJECT_HANDLE, RC_OK, Reader, Transport, find_jpeg, parse_device_info,
};
use crate::{Error, Format, Result, Source};

const OP_SDIO_CONNECT: u16 = 0x9201;
const OP_SDIO_GET_EXT_DEVICE_INFO: u16 = 0x9202;
const OP_SDIO_SET_EXT_DEVICE_PROP_VALUE: u16 = 0x9205;
/// Asks for protocol 3.00, and older bodies answer with the newest they have.
const PROTOCOL_3_00: u32 = 0x12C;
const PROP_PRIORITY_MODE: u16 = 0xD25A;
/// The object handle the live view frame is read from, on the Alpha bodies and the QX alike.
const LIVE_VIEW: u32 = 0xFFFF_C002;

fn check(op: u16, code: u16) -> Result<()> {
    if code == RC_OK {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "camera answered 0x{code:04X} to operation 0x{op:04X}"
        )))
    }
}

/// How many codes a `GetExtDeviceInfo` reply lists, after its version.
fn ext_codes(d: &[u8]) -> usize {
    let mut r = Reader { d };
    if r.take(2).is_none() {
        return 0;
    }
    let props = r.u16_array().map_or(0, |a| a.len());
    props + r.u16_array().map_or(0, |a| a.len())
}

/// The live view JPEG in a `GetObject` reply. The reply starts with the JPEG's offset, which
/// is trusted when it points at FF D8, since the header in front can hold FF D8 itself
/// (gphoto2 issue 389).
fn live_view_jpeg(d: &[u8]) -> Option<&[u8]> {
    let offset = d
        .get(..4)
        .filter(|_| d.len() > 4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, |b| u32::from_le_bytes(b) as usize);
    let start = if d.get(offset..offset.saturating_add(2)) == Some(&[0xFF, 0xD8]) {
        offset
    } else {
        0
    };
    find_jpeg(d.get(start..)?)
}

/// A Sony body streaming live view in PC Remote mode.
#[derive(Debug)]
pub struct Sony<T> {
    t: T,
    name: String,
    buf: Vec<u8>,
}

impl<T: Transport> Sony<T> {
    /// Runs the SDIO handshake that puts the body under the PC's control.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Unsupported`] when the body isn't in PC Remote mode, or with what the
    /// camera answered.
    pub fn open(mut t: T) -> Result<Self> {
        let mut buf = Vec::new();
        check(
            OP_GET_DEVICE_INFO,
            t.read(OP_GET_DEVICE_INFO, &[], &mut buf)?,
        )?;
        let DeviceInfo { ops, model, .. } = parse_device_info(&buf)
            .ok_or_else(|| Error::Failed("the camera sent a broken DeviceInfo".into()))?;
        let name = if model.starts_with("Sony") {
            model
        } else {
            format!("Sony {model}")
        };
        if !ops.contains(&OP_SDIO_GET_EXT_DEVICE_INFO) {
            return Err(Error::Unsupported(format!(
                "The {name} needs its USB connection set to PC Remote for live view."
            )));
        }
        for phase in [1, 2] {
            check(
                OP_SDIO_CONNECT,
                t.read(OP_SDIO_CONNECT, &[phase, 0, 0], &mut buf)?,
            )?;
        }
        // The list comes back empty for a while after connecting.
        for _ in 0..20 {
            let code = t.read(OP_SDIO_GET_EXT_DEVICE_INFO, &[PROTOCOL_3_00, 1], &mut buf)?;
            check(OP_SDIO_GET_EXT_DEVICE_INFO, code)?;
            if ext_codes(&buf) > 0 {
                break;
            }
        }
        check(
            OP_SDIO_CONNECT,
            t.read(OP_SDIO_CONNECT, &[3, 0, 0], &mut buf)?,
        )?;
        // Puts the PC in charge rather than the body's dials. gphoto only logs a refusal.
        t.write(
            OP_SDIO_SET_EXT_DEVICE_PROP_VALUE,
            &[PROP_PRIORITY_MODE.into()],
            &[1],
        )?;
        Ok(Self { t, name, buf })
    }
}

impl<T: Transport + Send> Source for Sony<T> {
    fn model(&self) -> &str {
        &self.name
    }

    fn start(&mut self) -> Result<()> {
        Ok(())
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the camera thread, and gphoto backs off for the body the same way"
    )]
    fn frame(&mut self, out: &mut Vec<u8>) -> Result<Option<Format>> {
        // The frame can be missing for up to a second on a ZV-1.
        let code = self
            .t
            .read(OP_GET_OBJECT_INFO, &[LIVE_VIEW], &mut self.buf)?;
        if code == RC_INVALID_OBJECT_HANDLE {
            sleep(Duration::from_millis(50));
            return Ok(None);
        }
        match self.t.read(OP_GET_OBJECT, &[LIVE_VIEW], &mut self.buf)? {
            RC_OK => {
                let jpeg = live_view_jpeg(&self.buf).ok_or_else(|| {
                    Error::Failed("the camera's live view frame held no JPEG".into())
                })?;
                out.clear();
                out.extend_from_slice(jpeg);
                Ok(Some(Format::Jpeg))
            }
            // Asking too fast.
            RC_ACCESS_DENIED => {
                sleep(Duration::from_millis(20));
                Ok(None)
            }
            code => check(OP_GET_OBJECT, code).map(|()| None),
        }
    }

    fn close(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::tests::{Script, Step, device_info, step, tricky_jpeg, u16s};

    fn handshake(ext_tries: usize) -> Vec<Step> {
        let info = device_info(0x11, &[0x1001, 0x9202], &[], "Sony Corporation", "ILCE-7M3");
        let mut ext = 300u16.to_le_bytes().to_vec();
        ext.extend(u16s(&[0x5004, 0x500B]));
        ext.extend(u16s(&[0xD2C1]));
        let mut steps = vec![
            step(OP_GET_DEVICE_INFO, &[], RC_OK).reply(&info),
            step(OP_SDIO_CONNECT, &[1, 0, 0], RC_OK),
            step(OP_SDIO_CONNECT, &[2, 0, 0], RC_OK),
        ];
        for _ in 1..ext_tries {
            steps.push(step(OP_SDIO_GET_EXT_DEVICE_INFO, &[0x12C, 1], RC_OK));
        }
        steps.extend([
            step(OP_SDIO_GET_EXT_DEVICE_INFO, &[0x12C, 1], RC_OK).reply(&ext),
            step(OP_SDIO_CONNECT, &[3, 0, 0], RC_OK),
            step(OP_SDIO_SET_EXT_DEVICE_PROP_VALUE, &[0xD25A], RC_OK).sent(&[1]),
        ]);
        steps
    }

    /// A reply whose header holds a stray FF D8 before the offset it gives.
    fn reply(jpeg: &[u8]) -> Vec<u8> {
        let mut d = 16u32.to_le_bytes().to_vec();
        d.extend([0, 0, 0, 0, 0xFF, 0xD8, 0xFF, 0xD9, 0, 0, 0, 0]);
        d.extend(jpeg);
        d.extend([0; 8]);
        d
    }

    #[test]
    fn a_full_session() {
        let jpeg = tricky_jpeg();
        let mut steps = handshake(3);
        steps.extend([
            step(OP_GET_OBJECT_INFO, &[LIVE_VIEW], RC_INVALID_OBJECT_HANDLE),
            step(OP_GET_OBJECT_INFO, &[LIVE_VIEW], RC_OK),
            step(OP_GET_OBJECT, &[LIVE_VIEW], RC_ACCESS_DENIED),
            step(OP_GET_OBJECT_INFO, &[LIVE_VIEW], 0x2002),
            step(OP_GET_OBJECT, &[LIVE_VIEW], RC_OK).reply(&reply(&jpeg)),
            step(OP_GET_OBJECT_INFO, &[LIVE_VIEW], RC_OK),
            step(OP_GET_OBJECT, &[LIVE_VIEW], 0x2002),
        ]);
        let mut cam = Box::new(Sony::open(Script::new(steps)).unwrap());
        assert_eq!(cam.model(), "Sony ILCE-7M3");
        cam.start().unwrap();
        let mut out = Vec::new();
        assert_eq!(cam.frame(&mut out).unwrap(), None);
        assert_eq!(cam.frame(&mut out).unwrap(), None);
        assert_eq!(cam.frame(&mut out).unwrap(), Some(Format::Jpeg));
        assert_eq!(out, jpeg);
        assert!(cam.frame(&mut out).is_err());
        assert!(cam.t.done());
        cam.close().unwrap();
    }

    #[test]
    fn an_offset_that_misses_falls_back_to_a_search() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xD9];
        let mut d = 1000u32.to_le_bytes().to_vec();
        d.extend([1, 2]);
        d.extend(jpeg);
        assert_eq!(live_view_jpeg(&d), Some(&jpeg[..]));
        let mut d = 4u32.to_le_bytes().to_vec();
        d.extend(jpeg);
        assert_eq!(live_view_jpeg(&d), Some(&jpeg[..]));
        assert_eq!(live_view_jpeg(&[0xFF, 0xD8, 0xFF]), None);
    }

    #[test]
    fn mtp_mode_is_turned_away() {
        let info = device_info(6, &[0x1001], &[], "Sony Corporation", "ILCE-7M3");
        let steps = [step(OP_GET_DEVICE_INFO, &[], RC_OK).reply(&info)];
        assert_eq!(
            Sony::open(Script::new(steps)).map(drop),
            Err(Error::Unsupported(
                "The Sony ILCE-7M3 needs its USB connection set to PC Remote for live view.".into()
            ))
        );
    }
}
