//! Nikon live view over PTP, following the `PTP_VENDOR_NIKON` case of `camera_capture_preview`
//! and the Nikon fixups of `fixup_cached_deviceinfo` in gphoto's library.c.
//!
//! Untested on a real body. Every step and code is gphoto's.

use std::thread::sleep;
use std::time::Duration;

use crate::ptp::{
    DeviceInfo, OP_GET_DEVICE_INFO, OP_GET_DEVICE_PROP_VALUE, OP_SET_DEVICE_PROP_VALUE,
    RC_DEVICE_BUSY, RC_OK, Reader, Transport, find_jpeg, parse_device_info,
};
use crate::{Error, Format, Result, Source};

const OP_CHANGE_CAMERA_MODE: u16 = 0x90C2;
const OP_DEVICE_READY: u16 = 0x90C8;
const OP_GET_VENDOR_PROP_CODES: u16 = 0x90CA;
const OP_START_LIVE_VIEW: u16 = 0x9201;
const OP_END_LIVE_VIEW: u16 = 0x9202;
const OP_GET_LIVE_VIEW_IMG: u16 = 0x9203;
const OP_INITIATE_CAPTURE_REC_IN_MEDIA: u16 = 0x9207;

const RC_CHANGE_CAMERA_MODE_FAILED: u16 = 0xA003;
const RC_INVALID_STATUS: u16 = 0xA004;
const RC_NOT_LIVE_VIEW: u16 = 0xA00B;
const RC_BULB_RELEASE_BUSY: u16 = 0xA200;
const RC_SILENT_RELEASE_BUSY: u16 = 0xA201;

const PROP_RECORDING_MEDIA: u16 = 0xD10B;
const PROP_LIVE_VIEW_STATUS: u16 = 0xD1A2;
const PROP_LIVE_VIEW_PROHIBIT: u16 = 0xD1A4;

/// Why live view won't start, by `LiveViewProhibitCondition` bit, in gphoto's order.
const PROHIBIT: [(u32, &str); 16] = [
    (8, "the battery is exhausted"),
    (17, "the camera is too hot"),
    (9, "TTL error"),
    (22, "the mirror is up"),
    (5, "minimum aperture warning"),
    (15, "a shot is still being processed"),
    (2, "sequence error"),
    (31, "the exposure mode isn't P, A, S or M"),
    (21, "bulb warning"),
    (20, "the card isn't formatted"),
    (19, "card error"),
    (18, "the card is locked"),
    (
        14,
        "it records to a card, and there's no card or it's locked",
    ),
    (12, "an image in the camera's memory hasn't been taken off"),
    (4, "the shutter button is fully pressed"),
    (24, "the lens is retracting"),
];

/// The operations gphoto adds for bodies that hide them, as far as live view goes.
fn fix_ops(model: &str, ops: &mut Vec<u16>) {
    let has = |ops: &[u16], op| ops.contains(&op);
    let add = |ops: &mut Vec<u16>, list: &[u16]| {
        for &op in list {
            if !ops.contains(&op) {
                ops.push(op);
            }
        }
    };
    let first = model.chars().next();
    // Nikon 1: the J and V series, and S1 or S2 but not an S7000.
    if matches!(first, Some('J' | 'V')) || (first == Some('S') && model.len() < 3) {
        if !has(ops, OP_INITIATE_CAPTURE_REC_IN_MEDIA) && has(ops, OP_START_LIVE_VIEW) {
            add(ops, &[OP_INITIATE_CAPTURE_REC_IN_MEDIA]);
        }
        if has(ops, OP_INITIATE_CAPTURE_REC_IN_MEDIA) {
            add(ops, &[OP_GET_VENDOR_PROP_CODES]);
        }
        if !["V1", "S1", "J1", "J3", "J4"].contains(&model) {
            add(ops, &[OP_CHANGE_CAMERA_MODE]);
        }
    }
    let live = [
        OP_CHANGE_CAMERA_MODE,
        OP_DEVICE_READY,
        OP_START_LIVE_VIEW,
        OP_END_LIVE_VIEW,
        OP_GET_LIVE_VIEW_IMG,
    ];
    let hidden = !has(ops, OP_GET_VENDOR_PROP_CODES);
    if model == "COOLPIX A" && hidden {
        add(ops, &[OP_GET_VENDOR_PROP_CODES]);
        add(ops, &live);
    }
    let digits: String = model
        .strip_prefix('D')
        .unwrap_or_default()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    match digits.parse::<u32>() {
        Ok(3000..3200) if hidden => add(ops, &[OP_DEVICE_READY]),
        Ok(3200..3300) if hidden => add(ops, &live),
        Ok(3300..4000) if hidden => {
            add(ops, &[OP_GET_VENDOR_PROP_CODES]);
            add(ops, &live);
        }
        _ => {}
    }
}

fn check(op: u16, code: u16) -> Result<()> {
    if code == RC_OK {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "camera answered 0x{code:04X} to operation 0x{op:04X}"
        )))
    }
}

/// A Nikon body streaming live view.
#[derive(Debug)]
pub struct Nikon<T> {
    t: T,
    info: DeviceInfo,
    name: String,
    buf: Vec<u8>,
    live: bool,
    control_mode: bool,
    /// The first frame after live view starts is corrupt on the S9700, so gphoto drops it.
    skip_next: bool,
}

impl<T: Transport> Nikon<T> {
    /// Reads what the camera can do and checks it has live view.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Unsupported`] when the body has no live view over USB, or with what
    /// the camera answered.
    pub fn open(mut t: T) -> Result<Self> {
        let mut buf = Vec::new();
        check(
            OP_GET_DEVICE_INFO,
            t.read(OP_GET_DEVICE_INFO, &[], &mut buf)?,
        )?;
        let mut info = parse_device_info(&buf)
            .ok_or_else(|| Error::Failed("the camera sent a broken DeviceInfo".into()))?;
        fix_ops(&info.model, &mut info.ops);
        // Newer bodies only list most of their properties here. A failure is only logged by
        // gphoto, so it's ignored.
        if info.ops.contains(&OP_GET_VENDOR_PROP_CODES)
            && t.read(OP_GET_VENDOR_PROP_CODES, &[], &mut buf)? == RC_OK
            && let Some(more) = (Reader { d: &buf }).u16_array()
        {
            info.props.extend(more);
        }
        let name = if info.model.starts_with("Nikon") {
            info.model.clone()
        } else {
            format!("Nikon {}", info.model)
        };
        if !info.ops.contains(&OP_START_LIVE_VIEW) {
            return Err(Error::Unsupported(format!(
                "The {name} has no live view over USB."
            )));
        }
        Ok(Self {
            t,
            info,
            name,
            buf,
            live: false,
            control_mode: false,
            skip_next: false,
        })
    }

    fn supports(&self, op: u16) -> bool {
        self.info.ops.contains(&op)
    }

    fn has_prop(&self, prop: u16) -> bool {
        self.info.props.contains(&prop)
    }

    fn prop(&mut self, prop: u16, width: usize) -> Result<Option<u32>> {
        let code = self
            .t
            .read(OP_GET_DEVICE_PROP_VALUE, &[prop.into()], &mut self.buf)?;
        let value = self
            .buf
            .get(..width)
            .map(|b| b.iter().rev().fold(0, |v, &x| (v << 8) | u32::from(x)));
        Ok(value.filter(|_| code == RC_OK))
    }

    /// `nikon_wait_busy(params, 20, 2000)`: polls `DeviceReady` until the body isn't busy.
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the camera thread, where waiting for the body is fine"
    )]
    fn wait_ready(&mut self) -> Result<()> {
        for _ in 0..=100 {
            match self.t.call(OP_DEVICE_READY, &[])? {
                RC_DEVICE_BUSY | RC_BULB_RELEASE_BUSY => sleep(Duration::from_millis(20)),
                RC_SILENT_RELEASE_BUSY => return Ok(()),
                code => return check(OP_DEVICE_READY, code),
            }
        }
        check(OP_DEVICE_READY, RC_DEVICE_BUSY)
    }

    fn start_live_view(&mut self) -> Result<()> {
        let code = self.t.call(OP_START_LIVE_VIEW, &[])?;
        if code != RC_DEVICE_BUSY {
            check(OP_START_LIVE_VIEW, code)?;
        }
        self.wait_ready()?;
        self.live = true;
        Ok(())
    }

    /// The `enable_liveview` label: record to the card, check nothing forbids live view, start it.
    fn enable(&mut self) -> Result<()> {
        if self.has_prop(PROP_RECORDING_MEDIA) {
            self.t.write(
                OP_SET_DEVICE_PROP_VALUE,
                &[PROP_RECORDING_MEDIA.into()],
                &[1],
            )?;
        }
        if self.has_prop(PROP_LIVE_VIEW_PROHIBIT) {
            let why = self.prop(PROP_LIVE_VIEW_PROHIBIT, 4)?.ok_or_else(|| {
                Error::Failed("the camera didn't say whether live view is allowed".into())
            })?;
            if why != 0 && why & (1 << 24) == 0 {
                let reason = PROHIBIT
                    .iter()
                    .find(|(bit, _)| why & (1 << bit) != 0)
                    .map_or_else(|| format!("code 0x{why:08X}"), |(_, r)| (*r).to_owned());
                return Err(Error::Unsupported(format!(
                    "Live view can't start: {reason}."
                )));
            }
        }
        self.start_live_view()?;
        self.skip_next = true;
        Ok(())
    }
}

impl<T: Transport + Send> Source for Nikon<T> {
    fn model(&self) -> &str {
        &self.name
    }

    fn start(&mut self) -> Result<()> {
        if !self.control_mode && self.supports(OP_CHANGE_CAMERA_MODE) {
            let code = self.t.call(OP_CHANGE_CAMERA_MODE, &[1])?;
            if code != RC_CHANGE_CAMERA_MODE_FAILED {
                check(OP_CHANGE_CAMERA_MODE, code)?;
            }
            self.control_mode = true;
        }
        if self.prop(PROP_LIVE_VIEW_STATUS, 1)?.unwrap_or(0) == 0 {
            self.enable()
        } else if self.live {
            Ok(())
        } else {
            // A Nikon 1 reports live view on before it has started.
            self.start_live_view()
        }
    }

    fn frame(&mut self, out: &mut Vec<u8>) -> Result<Option<Format>> {
        if !self.live {
            self.start()?;
        }
        match self.t.read(OP_GET_LIVE_VIEW_IMG, &[], &mut self.buf)? {
            RC_OK if self.skip_next => {
                self.skip_next = false;
                Ok(None)
            }
            RC_OK => {
                let jpeg = find_jpeg(&self.buf).ok_or_else(|| {
                    Error::Failed("the camera's live view frame held no JPEG".into())
                })?;
                out.clear();
                out.extend_from_slice(jpeg);
                Ok(Some(Format::Jpeg))
            }
            // A D7000 drops out of live view after about 14000 frames.
            RC_NOT_LIVE_VIEW => {
                self.live = false;
                self.enable().map(|()| None)
            }
            RC_DEVICE_BUSY | RC_INVALID_STATUS => Ok(None),
            code => check(OP_GET_LIVE_VIEW_IMG, code).map(|()| None),
        }
    }

    fn close(mut self: Box<Self>) -> Result<()> {
        let first = if self.supports(OP_END_LIVE_VIEW) {
            self.t
                .call(OP_END_LIVE_VIEW, &[])
                .and_then(|code| check(OP_END_LIVE_VIEW, code))
        } else {
            Ok(())
        };
        self.live = false;
        if self.control_mode && first != Err(Error::Disconnected) {
            // gphoto ignores how this goes.
            self.t.call(OP_CHANGE_CAMERA_MODE, &[0]).ok();
        }
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::tests::{Script, device_info, step, u16s};

    const NIKON: u32 = 0x0A;
    const ALL: [u16; 6] = [
        OP_CHANGE_CAMERA_MODE,
        OP_DEVICE_READY,
        OP_GET_VENDOR_PROP_CODES,
        OP_START_LIVE_VIEW,
        OP_END_LIVE_VIEW,
        OP_GET_LIVE_VIEW_IMG,
    ];

    fn ops_after_fixups(model: &str, ops: &[u16]) -> Vec<u16> {
        let mut ops = ops.to_vec();
        fix_ops(model, &mut ops);
        ops.sort_unstable();
        ops
    }

    #[test]
    fn fixups_add_the_hidden_live_view_ops() {
        let live = [0x90C2, 0x90C8, 0x9201, 0x9202, 0x9203];
        assert_eq!(ops_after_fixups("D3200", &[]), live);
        assert_eq!(
            ops_after_fixups("D3300", &[]),
            [&live[..2], &[0x90CA], &live[2..]].concat()
        );
        assert_eq!(ops_after_fixups("D3100", &[]), [0x90C8]);
        assert_eq!(
            ops_after_fixups("COOLPIX A", &[]),
            ops_after_fixups("D3300", &[])
        );
        // A body that lists its vendor codes is taken at its word.
        assert_eq!(ops_after_fixups("D3400", &[0x90CA]), [0x90CA]);
        assert_eq!(ops_after_fixups("D7000", &[0x9201]), [0x9201]);
        assert_eq!(ops_after_fixups("D300S", &[]), Vec::<u16>::new());
    }

    #[test]
    fn fixups_for_nikon_1() {
        assert_eq!(
            ops_after_fixups("J5", &[0x9201]),
            [0x90C2, 0x90CA, 0x9201, 0x9207]
        );
        assert_eq!(ops_after_fixups("V1", &[0x9201]), [0x90CA, 0x9201, 0x9207]);
        assert_eq!(ops_after_fixups("S1", &[]), Vec::<u16>::new());
        assert_eq!(ops_after_fixups("S7000", &[0x9201]), [0x9201]);
    }

    fn opened(
        ops: &[u16],
        props: &[u16],
        vendor_props: Option<&[u16]>,
    ) -> Vec<crate::ptp::tests::Step> {
        let mut steps = vec![step(OP_GET_DEVICE_INFO, &[], RC_OK).reply(&device_info(
            NIKON,
            ops,
            props,
            "Nikon Corporation",
            "D7000",
        ))];
        if let Some(v) = vendor_props {
            steps.push(step(OP_GET_VENDOR_PROP_CODES, &[], RC_OK).reply(&u16s(v)));
        }
        steps
    }

    /// The header in front of the JPEG is 384 bytes on a D7000. gphoto skips it by looking for
    /// the JPEG's start, so it's only filler here, with big-endian sizes like Nikon's.
    fn live_view_reply(jpeg: &[u8]) -> Vec<u8> {
        let mut d = vec![0; 384];
        d.splice(0..4, (jpeg.len() as u32).to_be_bytes());
        d.splice(4..8, [0x02, 0x80, 0x01, 0xE0]);
        d.extend(jpeg);
        d
    }

    #[test]
    fn a_full_session() {
        let jpeg = crate::ptp::tests::tricky_jpeg();
        let mut steps = opened(
            &ALL,
            &[PROP_LIVE_VIEW_STATUS],
            Some(&[PROP_RECORDING_MEDIA, PROP_LIVE_VIEW_PROHIBIT]),
        );
        let enable = || {
            [
                step(OP_SET_DEVICE_PROP_VALUE, &[0xD10B], RC_OK).sent(&[1]),
                step(OP_GET_DEVICE_PROP_VALUE, &[0xD1A4], RC_OK).reply(&[0, 0, 0, 0]),
                step(OP_START_LIVE_VIEW, &[], RC_OK),
                step(OP_DEVICE_READY, &[], RC_DEVICE_BUSY),
                step(OP_DEVICE_READY, &[], RC_OK),
            ]
        };
        steps.extend([
            step(OP_CHANGE_CAMERA_MODE, &[1], RC_OK),
            step(OP_GET_DEVICE_PROP_VALUE, &[0xD1A2], RC_OK).reply(&[0]),
        ]);
        steps.extend(enable());
        steps.extend([
            step(OP_GET_LIVE_VIEW_IMG, &[], RC_OK)
                .reply(&live_view_reply(&[0xFF, 0xD8, 0xFF, 0xD9])),
            step(OP_GET_LIVE_VIEW_IMG, &[], RC_OK).reply(&live_view_reply(&jpeg)),
            step(OP_GET_LIVE_VIEW_IMG, &[], RC_DEVICE_BUSY),
            step(OP_GET_LIVE_VIEW_IMG, &[], RC_INVALID_STATUS),
            step(OP_GET_LIVE_VIEW_IMG, &[], RC_NOT_LIVE_VIEW),
        ]);
        steps.extend(enable());
        steps.extend([
            step(OP_GET_LIVE_VIEW_IMG, &[], RC_OK).reply(&live_view_reply(&jpeg)),
            step(OP_GET_LIVE_VIEW_IMG, &[], RC_OK).reply(&live_view_reply(&jpeg)),
            step(OP_END_LIVE_VIEW, &[], RC_OK),
            step(OP_CHANGE_CAMERA_MODE, &[0], RC_OK),
        ]);
        let mut cam = Box::new(Nikon::open(Script::new(steps)).unwrap());
        assert_eq!(cam.model(), "Nikon D7000");
        cam.start().unwrap();
        let mut out = Vec::new();
        assert_eq!(
            cam.frame(&mut out).unwrap(),
            None,
            "the first frame is dropped"
        );
        assert_eq!(cam.frame(&mut out).unwrap(), Some(Format::Jpeg));
        assert_eq!(out, jpeg);
        assert_eq!(cam.frame(&mut out).unwrap(), None);
        assert_eq!(cam.frame(&mut out).unwrap(), None);
        assert_eq!(
            cam.frame(&mut out).unwrap(),
            None,
            "live view dropped and restarted"
        );
        assert_eq!(
            cam.frame(&mut out).unwrap(),
            None,
            "the first frame is dropped again"
        );
        assert_eq!(cam.frame(&mut out).unwrap(), Some(Format::Jpeg));
        assert_eq!(cam.t.0.len(), 2);
        cam.close().unwrap();
    }

    #[test]
    fn a_body_already_in_live_view_isnt_restarted() {
        let mut steps = opened(
            &[OP_START_LIVE_VIEW, OP_GET_LIVE_VIEW_IMG, OP_DEVICE_READY],
            &[],
            None,
        );
        steps.extend([
            step(OP_GET_DEVICE_PROP_VALUE, &[0xD1A2], RC_OK).reply(&[1]),
            step(OP_START_LIVE_VIEW, &[], RC_DEVICE_BUSY),
            step(OP_DEVICE_READY, &[], RC_SILENT_RELEASE_BUSY),
            step(OP_GET_DEVICE_PROP_VALUE, &[0xD1A2], RC_OK).reply(&[1]),
        ]);
        let mut cam = Box::new(Nikon::open(Script::new(steps)).unwrap());
        cam.start().unwrap();
        cam.start().unwrap();
        assert!(cam.t.done());
        cam.close().unwrap();
    }

    #[test]
    fn prohibit_conditions_name_the_reason() {
        let mut steps = opened(&ALL, &[PROP_LIVE_VIEW_PROHIBIT], Some(&[]));
        steps.extend([
            step(OP_CHANGE_CAMERA_MODE, &[1], RC_CHANGE_CAMERA_MODE_FAILED),
            step(OP_GET_DEVICE_PROP_VALUE, &[0xD1A2], 0x2002),
            step(OP_GET_DEVICE_PROP_VALUE, &[0xD1A4], RC_OK)
                .reply(&((1u32 << 20) | (1 << 4)).to_le_bytes()),
        ]);
        let mut cam = Box::new(Nikon::open(Script::new(steps)).unwrap());
        assert_eq!(
            cam.start(),
            Err(Error::Unsupported(
                "Live view can't start: the card isn't formatted.".into()
            ))
        );
    }

    #[test]
    fn a_retracting_lens_doesnt_stop_live_view() {
        let mut steps = opened(
            &[OP_START_LIVE_VIEW, OP_DEVICE_READY],
            &[PROP_LIVE_VIEW_PROHIBIT],
            None,
        );
        steps.extend([
            step(OP_GET_DEVICE_PROP_VALUE, &[0xD1A2], RC_OK).reply(&[0]),
            step(OP_GET_DEVICE_PROP_VALUE, &[0xD1A4], RC_OK).reply(&(1u32 << 24).to_le_bytes()),
            step(OP_START_LIVE_VIEW, &[], RC_OK),
            step(OP_DEVICE_READY, &[], RC_OK),
        ]);
        let mut cam = Box::new(Nikon::open(Script::new(steps)).unwrap());
        cam.start().unwrap();
        assert!(cam.t.done());
    }

    #[test]
    fn bodies_without_live_view_are_turned_away() {
        let steps = opened(&[0x1001], &[], None);
        assert_eq!(
            Nikon::open(Script::new(steps)).map(drop),
            Err(Error::Unsupported(
                "The Nikon D7000 has no live view over USB.".into()
            ))
        );
    }
}
