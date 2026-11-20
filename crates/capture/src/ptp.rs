//! The PTP transport, codes and parsing the Nikon and Sony backends share.

use crate::{Error, Result};

/// Sends PTP operations to one camera.
pub trait Transport {
    /// An operation with no data phase, returning the response code.
    /// # Errors
    /// Fails when the operation never reached the camera.
    fn call(&mut self, op: u16, params: &[u32]) -> Result<u16>;

    /// An operation that replaces `out` with its data, returning the response code.
    /// # Errors
    /// Fails when the operation never reached the camera.
    fn read(&mut self, op: u16, params: &[u32], out: &mut Vec<u8>) -> Result<u16>;

    /// An operation that sends `data`, returning the response code.
    /// # Errors
    /// Fails when the operation never reached the camera.
    fn write(&mut self, op: u16, params: &[u32], data: &[u8]) -> Result<u16>;
}

pub(crate) const OP_GET_DEVICE_INFO: u16 = 0x1001;
pub(crate) const OP_GET_OBJECT_INFO: u16 = 0x1008;
pub(crate) const OP_GET_OBJECT: u16 = 0x1009;
pub(crate) const OP_GET_DEVICE_PROP_VALUE: u16 = 0x1015;
pub(crate) const OP_SET_DEVICE_PROP_VALUE: u16 = 0x1016;

pub(crate) const RC_OK: u16 = 0x2001;
pub(crate) const RC_INVALID_OBJECT_HANDLE: u16 = 0x2009;
pub(crate) const RC_ACCESS_DENIED: u16 = 0x200F;
pub(crate) const RC_DEVICE_BUSY: u16 = 0x2019;

/// The parts of a PTP `DeviceInfo` dataset the backends use.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Operations the camera advertises.
    pub ops: Vec<u16>,
    /// Device properties the camera advertises.
    pub props: Vec<u16>,
    /// The model string.
    pub model: String,
}

pub(crate) fn check(op: u16, code: u16) -> Result<()> {
    if code == RC_OK {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "camera answered 0x{code:04X} to operation 0x{op:04X}"
        )))
    }
}

pub(crate) struct Reader<'a> {
    pub(crate) d: &'a [u8],
}

impl Reader<'_> {
    pub(crate) const fn take(&mut self, n: usize) -> Option<&[u8]> {
        if n > self.d.len() {
            return None;
        }
        let (head, rest) = self.d.split_at(n);
        self.d = rest;
        Some(head)
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    pub(crate) fn u16_array(&mut self) -> Option<Vec<u16>> {
        let n = self.u32()? as usize;
        let raw = self.take(n.checked_mul(2)?)?;
        Some(
            raw.as_chunks::<2>()
                .0
                .iter()
                .map(|&c| u16::from_le_bytes(c))
                .collect(),
        )
    }

    fn string(&mut self) -> Option<String> {
        let n = usize::from(*self.take(1)?.first()?);
        let units: Vec<u16> = self
            .take(n * 2)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .take_while(|&u| u != 0)
            .collect();
        Some(String::from_utf16_lossy(&units))
    }
}

/// Parses a `GetDeviceInfo` (0x1001) dataset up to the model string.
#[must_use]
pub fn parse_device_info(d: &[u8]) -> Option<DeviceInfo> {
    let mut r = Reader { d };
    r.take(8)?;
    r.string()?;
    r.take(2)?;
    let ops = r.u16_array()?;
    r.u16_array()?;
    let props = r.u16_array()?;
    r.u16_array()?;
    r.u16_array()?;
    r.string()?;
    let model = r.string()?;
    Some(DeviceInfo { ops, props, model })
}

/// Where the JPEG at `start` ends, as gphoto's `jpeg_segments_end` finds it. Table segments
/// aren't byte stuffed and can hold FF D9, so the first FF D9 isn't always the end.
fn segments_end(d: &[u8], start: usize) -> Option<usize> {
    let at = |i: usize| d.get(i).copied();
    let mut p = start + 2;
    while p < d.len() {
        if at(p)? != 0xFF {
            return None;
        }
        while at(p) == Some(0xFF) {
            p += 1;
        }
        let marker = at(p)?;
        p += 1;
        if marker == 0xD9 {
            return Some(p);
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        let len = usize::from(u16::from_be_bytes([at(p)?, at(p + 1)?]));
        if len < 2 || len > d.len() - p {
            return None;
        }
        p += len;
        if marker == 0xDA {
            // Entropy-coded data, where every FF is stuffed with 00, a restart or fill.
            while p + 1 < d.len() {
                let next = at(p + 1)?;
                if at(p)? == 0xFF && next != 0 && next != 0xFF && !(0xD0..=0xD7).contains(&next) {
                    break;
                }
                p += 1;
            }
            if p + 1 >= d.len() {
                return None;
            }
        }
    }
    None
}

/// The JPEG inside a live view reply, as gphoto's `save_jpeg_in_data_to_preview` finds it.
#[must_use]
pub fn find_jpeg(d: &[u8]) -> Option<&[u8]> {
    let start = d.windows(2).position(|w| w == [0xFF, 0xD8])?;
    let end = segments_end(d, start).or_else(|| {
        let after = start + 1;
        let i = d.get(after..)?.windows(2).position(|w| w == [0xFF, 0xD9])?;
        Some(after + i + 2)
    })?;
    d.get(start..end)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Debug)]
    pub(crate) struct Step {
        pub op: u16,
        pub params: Vec<u32>,
        pub sent: Vec<u8>,
        pub reply: Vec<u8>,
        pub code: u16,
    }

    pub(crate) fn step(op: u16, params: &[u32], code: u16) -> Step {
        Step {
            op,
            params: params.to_vec(),
            sent: Vec::new(),
            reply: Vec::new(),
            code,
        }
    }

    impl Step {
        pub(crate) fn reply(mut self, data: &[u8]) -> Self {
            self.reply = data.to_vec();
            self
        }

        pub(crate) fn sent(mut self, data: &[u8]) -> Self {
            self.sent = data.to_vec();
            self
        }
    }

    #[derive(Debug, Default)]
    pub(crate) struct Script(pub VecDeque<Step>);

    impl Script {
        pub(crate) fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            Self(steps.into_iter().collect())
        }

        fn next(&mut self, op: u16, params: &[u32], data: &[u8]) -> Step {
            let s = self.0.pop_front().unwrap_or_else(|| {
                panic!("unexpected 0x{op:04X} {params:X?} after the script ended")
            });
            assert_eq!(
                (s.op, s.params.as_slice(), s.sent.as_slice()),
                (op, params, data),
                "wrong operation"
            );
            s
        }

        pub(crate) fn done(&self) -> bool {
            self.0.is_empty()
        }
    }

    impl Transport for Script {
        fn call(&mut self, op: u16, params: &[u32]) -> Result<u16> {
            Ok(self.next(op, params, &[]).code)
        }

        fn read(&mut self, op: u16, params: &[u32], out: &mut Vec<u8>) -> Result<u16> {
            let s = self.next(op, params, &[]);
            out.clone_from(&s.reply);
            Ok(s.code)
        }

        fn write(&mut self, op: u16, params: &[u32], data: &[u8]) -> Result<u16> {
            Ok(self.next(op, params, data).code)
        }
    }

    pub(crate) fn ptp_string(s: &str) -> Vec<u8> {
        if s.is_empty() {
            return vec![0];
        }
        let units: Vec<u16> = s.encode_utf16().chain([0]).collect();
        let mut v = vec![units.len() as u8];
        v.extend(units.iter().flat_map(|u| u.to_le_bytes()));
        v
    }

    pub(crate) fn u16s(list: &[u16]) -> Vec<u8> {
        let mut v = (list.len() as u32).to_le_bytes().to_vec();
        v.extend(list.iter().flat_map(|c| c.to_le_bytes()));
        v
    }

    pub(crate) fn device_info(
        vendor: u32,
        ops: &[u16],
        props: &[u16],
        make: &str,
        model: &str,
    ) -> Vec<u8> {
        let mut d = vec![100, 0];
        d.extend(vendor.to_le_bytes());
        d.extend([100, 0]);
        d.extend(ptp_string(""));
        d.extend([0, 0]);
        d.extend(u16s(ops));
        d.extend(u16s(&[]));
        d.extend(u16s(props));
        d.extend(u16s(&[]));
        d.extend(u16s(&[0x3801]));
        d.extend(ptp_string(make));
        d.extend(ptp_string(model));
        d.extend(ptp_string("1.00"));
        d
    }

    #[test]
    fn device_info_reads_vendor_ops_props_and_names() {
        let d = device_info(
            0x0A,
            &[0x9201, 0x9203],
            &[0xD1A2],
            "Nikon Corporation",
            "D7000",
        );
        let info = parse_device_info(&d).unwrap();
        assert_eq!(
            info,
            DeviceInfo {
                ops: vec![0x9201, 0x9203],
                props: vec![0xD1A2],
                model: "D7000".into(),
            }
        );
        for cut in 0..d.len() - 12 {
            assert_eq!(parse_device_info(d.get(..cut).unwrap()), None, "cut {cut}");
        }
    }

    /// A small but well formed JPEG whose quantisation table holds FF D9.
    pub(crate) fn tricky_jpeg() -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        j.extend([0xFF, 0xDB, 0x00, 0x06, 0x00, 0xFF, 0xD9, 0x01]);
        j.extend([0xFF, 0xC0, 0x00, 0x0B, 8, 0, 2, 0, 3, 1, 1, 0x11, 0]);
        j.extend([0xFF, 0xDA, 0x00, 0x08, 1, 1, 0, 0, 0x3F, 0]);
        j.extend([0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56, 0xFF, 0xFF, 0xD9]);
        j
    }

    #[test]
    fn find_jpeg_walks_segments_past_a_false_end() {
        let jpeg = tricky_jpeg();
        let mut d = vec![0x80, 0, 0, 0, 0xFF, 0x12];
        d.extend(&jpeg);
        d.extend([0xAA; 16]);
        assert_eq!(find_jpeg(&d), Some(&jpeg[..]));
    }

    #[test]
    fn find_jpeg_falls_back_to_the_first_end_marker() {
        let d = [
            1, 2, 0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0xFF, 0x55, 0xFF, 0xD9, 7, 7,
        ];
        assert_eq!(find_jpeg(&d), Some(&d[2..11]));
    }

    #[test]
    fn find_jpeg_needs_a_start_and_an_end() {
        assert_eq!(find_jpeg(&[]), None);
        assert_eq!(find_jpeg(&[0xFF]), None);
        assert_eq!(find_jpeg(&[0xFF, 0xD9, 0xFF, 0xD8]), None);
        assert_eq!(find_jpeg(&[0, 0xFF, 0xD8, 1, 2, 3]), None);
        assert_eq!(
            find_jpeg(&[0xFF, 0xD8, 0xFF, 0xD9]),
            Some(&[0xFF, 0xD8, 0xFF, 0xD9][..])
        );
    }
}
