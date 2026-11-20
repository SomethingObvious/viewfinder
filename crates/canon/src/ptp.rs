//! PTP codes and the pure encoders and decoders for Canon's EOS vendor extension.

pub(crate) const OP_GET_DEVICE_INFO: u16 = 0x1001;
pub(crate) const OP_OPEN_SESSION: u16 = 0x1002;
pub(crate) const OP_SET_PROP_EX: u16 = 0x9110;
pub(crate) const OP_SET_REMOTE_MODE: u16 = 0x9114;
pub(crate) const OP_SET_EVENT_MODE: u16 = 0x9115;
pub(crate) const OP_GET_EVENT: u16 = 0x9116;
pub(crate) const OP_KEEP_DEVICE_ON: u16 = 0x911D;
pub(crate) const OP_REQUEST_OLC_INFO_GROUP: u16 = 0x913D;
pub(crate) const OP_GET_VIEWFINDER_DATA: u16 = 0x9153;
pub(crate) const OP_DO_AF: u16 = 0x9154;
pub(crate) const OP_DRIVE_LENS: u16 = 0x9155;
pub(crate) const OP_AF_CANCEL: u16 = 0x9160;

pub(crate) const RC_OK: u16 = 0x2001;
pub(crate) const RC_SESSION_NOT_OPEN: u16 = 0x2003;
pub(crate) const RC_OPERATION_NOT_SUPPORTED: u16 = 0x2005;
pub(crate) const RC_DEVICE_BUSY: u16 = 0x2019;
pub(crate) const RC_CANON_NOT_READY: u16 = 0xA102;

const EC_PROP_VALUE_CHANGED: u32 = 0xC189;
const EC_AVAIL_LIST_CHANGED: u32 = 0xC18A;

pub(crate) const fn response_name(code: u16) -> &'static str {
    match code {
        0x2001 => "OK",
        0x2002 => "GeneralError",
        0x2003 => "SessionNotOpen",
        0x2005 => "OperationNotSupported",
        0x2006 => "ParameterNotSupported",
        0x2019 => "DeviceBusy",
        0x201C => "InvalidDevicePropValue",
        0x201E => "SessionAlreadyOpen",
        0xA001 => "UnknownCommand",
        0xA005 => "OperationRefused",
        0xA006 => "LensCoverClosed",
        0xA101 => "LowBattery",
        0xA102 => "NotReady",
        _ => "",
    }
}

/// A camera setting, named after its 0xD1xx EOS property code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Prop {
    /// 0xD101, see `values::aperture_label`.
    Aperture,
    /// 0xD102, see `values::shutter_label`.
    Shutter,
    /// 0xD103, see `values::iso_label`.
    Iso,
    /// 0xD104, a signed byte in eighths of a stop.
    ExposureComp,
    /// 0xD105, the exposure mode the camera is running.
    AeMode,
    /// 0xD106, single, continuous or self-timer.
    DriveMode,
    /// 0xD107, metering pattern.
    Metering,
    /// 0xD108, the lens AF/MF switch as the body sees it.
    FocusMode,
    /// 0xD109, see `values::white_balance_label`.
    WhiteBalance,
    /// 0xD10A, Kelvin, used when white balance is set to colour temperature.
    ColorTemp,
    /// 0xD110, see `values::picture_style_label`.
    PictureStyle,
    /// 0xD111, see `values::battery_label`.
    Battery,
    /// 0xD114, seconds before the body sleeps.
    AutoPowerOff,
    /// 0xD11B, shots left on the card.
    AvailableShots,
    /// 0xD11C, card or host.
    CaptureDestination,
    /// 0xD138, the physical mode dial.
    AeModeDial,
    /// 0xD1B0, live view output mask: 1 is the rear screen, 2 is the PC.
    EvfOutput,
    /// 0xD1B1, live view on (1) or off (0).
    EvfMode,
    /// 0xD1B7, whether live view simulates the exposure.
    ExposureSim,
    /// 0xD1BA, live view AF method, see `values::af_method_label`.
    AfMethod,
    /// 0xD1C1, Auto Lighting Optimizer, where bit 16 keeps it on in M (`canon_eos_alomode`).
    Alo,
}

impl Prop {
    /// Every property this crate tracks.
    pub const ALL: [Self; 21] = [
        Self::Aperture,
        Self::Shutter,
        Self::Iso,
        Self::ExposureComp,
        Self::AeMode,
        Self::DriveMode,
        Self::Metering,
        Self::FocusMode,
        Self::WhiteBalance,
        Self::ColorTemp,
        Self::PictureStyle,
        Self::Battery,
        Self::AutoPowerOff,
        Self::AvailableShots,
        Self::CaptureDestination,
        Self::AeModeDial,
        Self::EvfOutput,
        Self::EvfMode,
        Self::ExposureSim,
        Self::AfMethod,
        Self::Alo,
    ];

    /// The PTP device property code.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::Aperture => 0xD101,
            Self::Shutter => 0xD102,
            Self::Iso => 0xD103,
            Self::ExposureComp => 0xD104,
            Self::AeMode => 0xD105,
            Self::DriveMode => 0xD106,
            Self::Metering => 0xD107,
            Self::FocusMode => 0xD108,
            Self::WhiteBalance => 0xD109,
            Self::ColorTemp => 0xD10A,
            Self::PictureStyle => 0xD110,
            Self::Battery => 0xD111,
            Self::AutoPowerOff => 0xD114,
            Self::AvailableShots => 0xD11B,
            Self::CaptureDestination => 0xD11C,
            Self::AeModeDial => 0xD138,
            Self::EvfOutput => 0xD1B0,
            Self::EvfMode => 0xD1B1,
            Self::ExposureSim => 0xD1B7,
            Self::AfMethod => 0xD1BA,
            Self::Alo => 0xD1C1,
        }
    }

    /// The property with this PTP code, if it's one we track.
    #[must_use]
    pub fn from_code(code: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.code() == code)
    }

    /// Value width in bytes, as `ptp_unpack_EOS_events` in ptp-pack.c assigns each `DataType`.
    pub(crate) const fn width(self) -> usize {
        match self {
            Self::Aperture
            | Self::Shutter
            | Self::Iso
            | Self::AeMode
            | Self::DriveMode
            | Self::Battery
            | Self::AeModeDial
            | Self::EvfMode => 2,
            Self::ExposureComp | Self::Metering | Self::WhiteBalance | Self::PictureStyle => 1,
            _ => 4,
        }
    }

    pub(crate) const fn mask(self) -> u32 {
        match self.width() {
            1 => 0xFF,
            2 => 0xFFFF,
            _ => u32::MAX,
        }
    }

    pub(crate) const fn index(self) -> usize {
        self as usize
    }
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    let b = d.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(b.try_into().ok()?))
}

fn read_le(d: &[u8], width: usize) -> Option<u32> {
    let b = d.get(..width)?;
    Some(b.iter().rev().fold(0, |v, &x| (v << 8) | u32::from(x)))
}

#[derive(Debug, PartialEq, Eq)]
#[expect(
    variant_size_differences,
    reason = "events live only while the reply is walked, so boxing the slice would cost more"
)]
pub(crate) enum Event<'a> {
    Value(Prop, u32),
    /// 4 bytes a value.
    Allowed(Prop, &'a [u8]),
    Unnamed(u16, u32),
    Other,
}

pub(crate) const fn events(data: &[u8]) -> Events<'_> {
    Events { data }
}

pub(crate) struct Events<'a> {
    data: &'a [u8],
}

impl<'a> Iterator for Events<'a> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Event<'a>> {
        let size = u32_at(self.data, 0)? as usize;
        let code = u32_at(self.data, 4)?;
        // The same stop rules as ptp_unpack_EOS_events.
        if size < 8 || size > self.data.len() || (size == 8 && code == 0) {
            self.data = &[];
            return None;
        }
        let (record, rest) = self.data.split_at(size);
        self.data = rest;
        Some(decode(code, record))
    }
}

fn decode(code: u32, record: &[u8]) -> Event<'_> {
    if code != EC_PROP_VALUE_CHANGED && code != EC_AVAIL_LIST_CHANGED {
        return Event::Other;
    }
    let Some(code16) = u32_at(record, 8).and_then(|c| u16::try_from(c).ok()) else {
        return Event::Other;
    };
    let Some(prop) = Prop::from_code(code16) else {
        if code != EC_PROP_VALUE_CHANGED {
            return Event::Other;
        }
        let value = record
            .get(12..)
            .and_then(|d| read_le(d, d.len().min(4)))
            .unwrap_or(0);
        return Event::Unnamed(code16, value);
    };
    if code == EC_PROP_VALUE_CHANGED {
        return record
            .get(12..)
            .and_then(|d| read_le(d, prop.width()))
            .map_or(Event::Other, |v| Event::Value(prop, v));
    }
    // Only list type 3 is an enumeration, 4 bytes an entry whatever the width (gphoto).
    if u32_at(record, 12) != Some(3) {
        return Event::Other;
    }
    let (Some(count), Some(list)) = (u32_at(record, 16), record.get(20..)) else {
        return Event::Other;
    };
    match (count as usize).checked_mul(4).and_then(|n| list.get(..n)) {
        Some(list) if count > 0 => Event::Allowed(prop, list),
        _ => Event::Other,
    }
}

pub(crate) fn allowed_values(prop: Prop, list: &[u8]) -> impl Iterator<Item = u32> + '_ {
    list.as_chunks::<4>()
        .0
        .iter()
        .map(move |&c| u32::from_le_bytes(c) & prop.mask())
}

pub(crate) fn viewfinder_jpeg(mut data: &[u8]) -> Option<&[u8]> {
    while let (Some(len), Some(kind)) = (u32_at(data, 0), u32_at(data, 4)) {
        // A zero length would spin forever, and camera_capture_preview has no guard for it.
        if len < 8 {
            return None;
        }
        // An overlong block is clamped to what arrived, like camera_capture_preview in library.c.
        let len = (len as usize).min(data.len());
        let payload = data.get(8..len)?;
        let jpeg = match kind {
            1 | 11 => true,
            // Movie mode sends type 9, which gphoto treats as raw. Take it only if it's a JPEG.
            9 => payload.starts_with(&[0xFF, 0xD8]),
            _ => false,
        };
        if jpeg && !payload.is_empty() {
            return Some(payload);
        }
        data = data.get(len..).unwrap_or_default();
    }
    None
}

pub(crate) fn set_prop_payload(prop: Prop, value: u32) -> [u8; 12] {
    let mut b = [0; 12];
    b[0..4].copy_from_slice(&12u32.to_le_bytes());
    b[4..8].copy_from_slice(&u32::from(prop.code()).to_le_bytes());
    b[8..12].copy_from_slice(&(value & prop.mask()).to_le_bytes());
    b
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DeviceInfo {
    pub model: String,
    pub ops: Vec<u16>,
}

struct Reader<'a> {
    d: &'a [u8],
}

impl Reader<'_> {
    const fn take(&mut self, n: usize) -> Option<&[u8]> {
        if n > self.d.len() {
            return None;
        }
        let (head, rest) = self.d.split_at(n);
        self.d = rest;
        Some(head)
    }

    fn u16_array(&mut self) -> Option<Vec<u16>> {
        let n = u32_at(self.take(4)?, 0)? as usize;
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
        let raw = self.take(n * 2)?;
        let units: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .take_while(|&u| u != 0)
            .collect();
        Some(String::from_utf16_lossy(&units))
    }
}

pub(crate) fn parse_device_info(d: &[u8]) -> Option<DeviceInfo> {
    let mut r = Reader { d };
    r.take(8)?;
    r.string()?;
    r.take(2)?;
    let ops = r.u16_array()?;
    for _ in 0..4 {
        r.u16_array()?;
    }
    r.string()?;
    let model = r.string()?;
    Some(DeviceInfo { model, ops })
}

/// A JPEG's width and height from its SOF marker.
#[must_use]
pub fn jpeg_size(d: &[u8]) -> Option<(u16, u16)> {
    if !d.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let be16 = |i: usize| Some(u16::from_be_bytes([*d.get(i)?, *d.get(i + 1)?]));
    let mut i = 2;
    loop {
        if *d.get(i)? != 0xFF {
            return None;
        }
        while *d.get(i)? == 0xFF {
            i += 1;
        }
        let marker = *d.get(i)?;
        i += 1;
        match marker {
            0x01 | 0xD0..=0xD7 => continue,
            0xD9 | 0xDA => return None,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                return Some((be16(i + 5)?, be16(i + 3)?));
            }
            _ => {}
        }
        let len = usize::from(be16(i)?);
        if len < 2 {
            return None;
        }
        i += len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(code: u32, body: &[u8]) -> Vec<u8> {
        let mut r = ((body.len() + 8) as u32).to_le_bytes().to_vec();
        r.extend_from_slice(&code.to_le_bytes());
        r.extend_from_slice(body);
        r
    }

    fn words(w: &[u32]) -> Vec<u8> {
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    const END: [u8; 8] = [8, 0, 0, 0, 0, 0, 0, 0];

    #[test]
    fn events_decode_values_lists_and_stop_at_terminator() {
        let mut d = record(
            EC_PROP_VALUE_CHANGED,
            &[&words(&[0xD103])[..], &[0x68, 0x00]].concat(),
        );
        d.extend(record(
            EC_PROP_VALUE_CHANGED,
            &words(&[0xD1B0, 0x8000_0002]),
        ));
        d.extend(record(
            EC_PROP_VALUE_CHANGED,
            &[&words(&[0xD109])[..], &[0x09, 0xAA, 0xBB]].concat(),
        ));
        d.extend(record(0xC181, &words(&[1, 2, 3])));
        d.extend(record(
            EC_AVAIL_LIST_CHANGED,
            &words(&[0xD103, 3, 3, 0, 0x48, 0x10068]),
        ));
        d.extend(END);
        d.extend(record(EC_PROP_VALUE_CHANGED, &words(&[0xD101, 0x30])));

        let got: Vec<_> = events(&d).collect();
        let [iso, evf, wb, other, allowed] = got.as_slice() else {
            panic!("{got:?}")
        };
        assert_eq!(*iso, Event::Value(Prop::Iso, 0x68));
        assert_eq!(*evf, Event::Value(Prop::EvfOutput, 0x8000_0002));
        assert_eq!(*wb, Event::Value(Prop::WhiteBalance, 9));
        assert_eq!(*other, Event::Other);
        let Event::Allowed(Prop::Iso, list) = *allowed else {
            panic!("{allowed:?}")
        };
        assert_eq!(
            allowed_values(Prop::Iso, list).collect::<Vec<_>>(),
            [0, 0x48, 0x68]
        );
    }

    #[test]
    fn events_reject_malformed_sizes() {
        let good = record(EC_PROP_VALUE_CHANGED, &words(&[0xD101, 0x30]));
        for bad_size in [0u32, 4, 7, 1000, u32::MAX] {
            let mut d = good.clone();
            d.extend(bad_size.to_le_bytes());
            d.extend(EC_PROP_VALUE_CHANGED.to_le_bytes());
            d.extend(words(&[0xD102, 0x60]));
            let got: Vec<_> = events(&d).collect();
            assert_eq!(got, [Event::Value(Prop::Aperture, 0x30)], "size {bad_size}");
        }
        assert_eq!(events(&[]).count(), 0);
        assert_eq!(events(&[1, 2, 3]).count(), 0);
        assert_eq!(events(&END).count(), 0);
    }

    #[test]
    fn events_skip_short_values_and_lists() {
        let mut d = record(EC_PROP_VALUE_CHANGED, &words(&[0xD101]));
        d.extend(record(
            EC_PROP_VALUE_CHANGED,
            &[&words(&[0xD1B0])[..], &[1, 0]].concat(),
        ));
        d.extend(record(EC_AVAIL_LIST_CHANGED, &words(&[0xD103, 3, 5, 0x48])));
        d.extend(record(
            EC_AVAIL_LIST_CHANGED,
            &words(&[0xD103, 3, u32::MAX, 0x48]),
        ));
        d.extend(record(EC_AVAIL_LIST_CHANGED, &words(&[0xD103, 7, 1, 0x48])));
        d.extend(record(EC_AVAIL_LIST_CHANGED, &words(&[0xD103, 3, 0])));
        d.extend(record(EC_PROP_VALUE_CHANGED, &words(&[0xFFFF_D101, 1])));
        d.extend(END);
        assert!(events(&d).all(|e| e == Event::Other));
        assert_eq!(events(&d).count(), 7);
    }

    #[test]
    fn viewfinder_finds_jpeg_after_unknown_blocks() {
        let jpeg = [0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9];
        let mut d = record(17, &[0; 32]);
        d.extend(record(5, &[9, 9]));
        d.extend(record(1, &jpeg));
        d.extend(record(11, &[0xFF, 0xD8, 0xEE]));
        assert_eq!(viewfinder_jpeg(&d), Some(&jpeg[..]));

        let d = record(11, &jpeg);
        assert_eq!(viewfinder_jpeg(&d), Some(&jpeg[..]));
    }

    #[test]
    fn viewfinder_movie_blocks_need_a_jpeg_header() {
        assert_eq!(viewfinder_jpeg(&record(9, &[1, 2, 3])), None);
        assert_eq!(
            viewfinder_jpeg(&record(9, &[0xFF, 0xD8, 3])),
            Some(&[0xFF, 0xD8, 3][..])
        );
    }

    #[test]
    fn viewfinder_clamps_truncated_record() {
        let mut d = 1000u32.to_le_bytes().to_vec();
        d.extend(1u32.to_le_bytes());
        d.extend([0xFF, 0xD8, 7, 7]);
        assert_eq!(viewfinder_jpeg(&d), Some(&[0xFF, 0xD8, 7, 7][..]));
    }

    #[test]
    fn viewfinder_without_jpeg_is_none() {
        assert_eq!(viewfinder_jpeg(&[]), None);
        assert_eq!(viewfinder_jpeg(&[1, 0, 0]), None);
        assert_eq!(viewfinder_jpeg(&record(17, &[0; 16])), None);
        assert_eq!(viewfinder_jpeg(&record(1, &[])), None);
        let mut zero = vec![0; 8];
        zero.extend(record(1, &[0xFF, 0xD8]));
        assert_eq!(viewfinder_jpeg(&zero), None);
    }

    #[test]
    fn set_prop_payload_is_size_code_value() {
        assert_eq!(
            set_prop_payload(Prop::EvfOutput, 2),
            [12, 0, 0, 0, 0xB0, 0xD1, 0, 0, 2, 0, 0, 0]
        );
        assert_eq!(
            set_prop_payload(Prop::Iso, 0x1_0068),
            [12, 0, 0, 0, 0x03, 0xD1, 0, 0, 0x68, 0, 0, 0]
        );
        assert_eq!(
            set_prop_payload(Prop::ExposureComp, 0xF8),
            [12, 0, 0, 0, 0x04, 0xD1, 0, 0, 0xF8, 0, 0, 0]
        );
    }

    fn ptp_string(s: &str) -> Vec<u8> {
        let units: Vec<u16> = s.encode_utf16().chain([0]).collect();
        let mut v = vec![units.len() as u8];
        v.extend(units.iter().flat_map(|u| u.to_le_bytes()));
        v
    }

    #[test]
    fn device_info_reads_ops_and_model() {
        let mut d = vec![100, 0, 11, 0, 0, 0, 100, 0];
        d.extend(ptp_string("canon.com: 1.0"));
        d.extend([0, 0]);
        d.extend(words(&[2]));
        d.extend([0x16, 0x91, 0x53, 0x91]);
        for _ in 0..4 {
            d.extend(words(&[0]));
        }
        d.extend(ptp_string("Canon Inc."));
        d.extend(ptp_string("Canon EOS 1100D"));
        let info = parse_device_info(&d).unwrap();
        assert_eq!(info.model, "Canon EOS 1100D");
        assert_eq!(info.ops, [0x9116, 0x9153]);

        for cut in 0..d.len() - 1 {
            assert_eq!(parse_device_info(d.get(..cut).unwrap()), None, "cut {cut}");
        }
        let mut huge = d.get(..8).unwrap().to_vec();
        huge.extend([0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(parse_device_info(&huge), None);
    }

    #[test]
    fn jpeg_size_reads_sof() {
        let mut j = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xFF];
        j.extend([0xC0, 0, 11, 8, 0x02, 0xC0, 0x04, 0x20, 3, 0, 0]);
        assert_eq!(jpeg_size(&j), Some((1056, 704)));

        let progressive = [0xFF, 0xD8, 0xFF, 0xC2, 0, 8, 8, 0, 2, 0, 3];
        assert_eq!(jpeg_size(&progressive), Some((3, 2)));
    }

    #[test]
    fn jpeg_size_rejects_junk() {
        assert_eq!(jpeg_size(&[]), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8]), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2]), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF]), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xC0, 0, 8, 8]), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0x12]), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xFF, 0xFF]), None);
    }
}
