//! Human labels for raw EOS property values, from the libgphoto2 config.c tables.

fn hex(v: u32) -> String {
    format!("0x{v:02x}")
}

fn find(table: &[(u32, &'static str)], v: u32) -> Option<&'static str> {
    table.iter().find(|e| e.0 == v).map(|e| e.1)
}

fn label(table: &[(u32, &'static str)], v: u32) -> String {
    find(table, v).map_or_else(|| hex(v), str::to_owned)
}

const ISO: &[(u32, &str)] = &[
    (0x00, "Auto"),
    (0x28, "6"),
    (0x30, "12"),
    (0x38, "25"),
    (0x40, "50"),
    (0x43, "64"),
    (0x45, "80"),
    (0x48, "100"),
    (0x4b, "125"),
    (0x4d, "160"),
    (0x50, "200"),
    (0x53, "250"),
    (0x55, "320"),
    (0x58, "400"),
    (0x5b, "500"),
    (0x5d, "640"),
    (0x60, "800"),
    (0x63, "1000"),
    (0x65, "1250"),
    (0x68, "1600"),
    (0x6b, "2000"),
    (0x6d, "2500"),
    (0x70, "3200"),
    (0x73, "4000"),
    (0x75, "5000"),
    (0x78, "6400"),
    (0x7b, "8000"),
    (0x7d, "10000"),
    (0x80, "12800"),
    (0x83, "16000"),
    (0x85, "20000"),
    (0x88, "25600"),
];

const APERTURE: &[(u32, &str)] = &[
    (0x08, "1"),
    (0x0b, "1.1"),
    (0x0c, "1.2"),
    (0x0d, "1.2"),
    (0x10, "1.4"),
    (0x13, "1.6"),
    (0x14, "1.8"),
    (0x15, "1.8"),
    (0x18, "2"),
    (0x1b, "2.2"),
    (0x1c, "2.5"),
    (0x1d, "2.5"),
    (0x20, "2.8"),
    (0x23, "3.2"),
    (0x24, "3.5"),
    (0x25, "3.5"),
    (0x28, "4"),
    (0x2b, "4.5"),
    (0x2c, "4.5"),
    (0x2d, "5"),
    (0x30, "5.6"),
    (0x33, "6.3"),
    (0x34, "6.7"),
    (0x35, "7.1"),
    (0x38, "8"),
    (0x3b, "9"),
    (0x3c, "9.5"),
    (0x3d, "10"),
    (0x40, "11"),
    (0x43, "13"),
    (0x44, "13"),
    (0x45, "14"),
    (0x48, "16"),
    (0x4b, "18"),
    (0x4c, "19"),
    (0x4d, "20"),
    (0x50, "22"),
    (0x53, "25"),
    (0x54, "27"),
    (0x55, "29"),
    (0x58, "32"),
    (0x5b, "36"),
    (0x5c, "38"),
    (0x5d, "40"),
    (0x60, "45"),
];

const SHUTTER: &[(u32, &str)] = &[
    (0x00, "Auto"),
    (0x04, "Bulb"),
    (0x0c, "Bulb"),
    (0x10, "30"),
    (0x13, "25"),
    (0x14, "20"),
    (0x15, "20"),
    (0x18, "15"),
    (0x1b, "13"),
    (0x1c, "10"),
    (0x1d, "10"),
    (0x20, "8"),
    (0x23, "6"),
    (0x24, "6"),
    (0x25, "5"),
    (0x28, "4"),
    (0x2b, "3.2"),
    (0x2c, "3"),
    (0x2d, "2.5"),
    (0x30, "2"),
    (0x33, "1.6"),
    (0x34, "1.5"),
    (0x35, "1.3"),
    (0x38, "1"),
    (0x3b, "0.8"),
    (0x3c, "0.7"),
    (0x3d, "0.6"),
    (0x40, "0.5"),
    (0x43, "0.4"),
    (0x44, "0.3"),
    (0x45, "0.3"),
    (0x48, "1/4"),
    (0x4b, "1/5"),
    (0x4c, "1/6"),
    (0x4d, "1/6"),
    (0x50, "1/8"),
    (0x53, "1/10"),
    (0x54, "1/10"),
    (0x55, "1/13"),
    (0x58, "1/15"),
    (0x5b, "1/20"),
    (0x5c, "1/20"),
    (0x5d, "1/25"),
    (0x60, "1/30"),
    (0x63, "1/40"),
    (0x64, "1/45"),
    (0x65, "1/50"),
    (0x68, "1/60"),
    (0x6b, "1/80"),
    (0x6c, "1/90"),
    (0x6d, "1/100"),
    (0x70, "1/125"),
    (0x73, "1/160"),
    (0x74, "1/180"),
    (0x75, "1/200"),
    (0x78, "1/250"),
    (0x7b, "1/320"),
    (0x7c, "1/350"),
    (0x7d, "1/400"),
    (0x80, "1/500"),
    (0x83, "1/640"),
    (0x84, "1/750"),
    (0x85, "1/800"),
    (0x88, "1/1000"),
    (0x8b, "1/1250"),
    (0x8c, "1/1500"),
    (0x8d, "1/1600"),
    (0x90, "1/2000"),
    (0x93, "1/2500"),
    (0x94, "1/3000"),
    (0x95, "1/3200"),
    (0x98, "1/4000"),
    (0x9b, "1/5000"),
    (0x9c, "1/6000"),
    (0x9d, "1/6400"),
    (0xa0, "1/8000"),
];

const WHITE_BALANCE: &[(u32, &str)] = &[
    (0, "Auto"),
    (1, "Daylight"),
    (2, "Cloudy"),
    (3, "Tungsten"),
    (4, "Fluorescent"),
    (5, "Flash"),
    (6, "Custom"),
    (8, "Shade"),
    (9, "Colour Temperature"),
    (10, "PC-1"),
    (11, "PC-2"),
    (12, "PC-3"),
    (15, "Custom 2"),
    (16, "Custom 3"),
    (18, "Custom 4"),
    (19, "Custom 5"),
    (20, "PC-4"),
    (21, "PC-5"),
    (23, "Auto White Priority"),
];

const PICTURE_STYLE: &[(u32, &str)] = &[
    (0x21, "User Defined 1"),
    (0x22, "User Defined 2"),
    (0x23, "User Defined 3"),
    (0x81, "Standard"),
    (0x82, "Portrait"),
    (0x83, "Landscape"),
    (0x84, "Neutral"),
    (0x85, "Faithful"),
    (0x86, "Monochrome"),
    (0x87, "Auto"),
    (0x88, "Fine Detail"),
];

const AE_MODE: &[(u32, &str)] = &[
    (0x00, "P"),
    (0x01, "Tv"),
    (0x02, "Av"),
    (0x03, "M"),
    (0x04, "Bulb"),
    (0x05, "A-DEP"),
    (0x06, "DEP"),
    (0x07, "Custom"),
    (0x08, "Lock"),
    (0x09, "Full Auto"),
    (0x0a, "Night Portrait"),
    (0x0b, "Sports"),
    (0x0c, "Portrait"),
    (0x0d, "Landscape"),
    (0x0e, "Close-up"),
    (0x0f, "Flash Off"),
    (0x10, "C2"),
    (0x11, "C3"),
    (0x13, "Creative Auto"),
    (0x14, "Movie"),
    (0x16, "Auto"),
    (0x19, "SCN"),
];

const AF_METHOD: &[(u32, &str)] = &[
    (0, "Quick"),
    (1, "Live"),
    (2, "Face Detection"),
    (3, "Live Multi"),
];

const BATTERY: &[(u32, &str)] = &[(0, "Low"), (1, "50%"), (2, "100%"), (4, "75%"), (5, "25%")];

/// ISO label, like "100" or "Auto".
#[must_use]
pub fn iso_label(v: u32) -> String {
    label(ISO, v)
}

/// Aperture label, like "f/5.6".
#[must_use]
pub fn aperture_label(v: u32) -> String {
    match v {
        0x00 | 0xb0 | 0xffff => "Auto".into(),
        _ => find(APERTURE, v).map_or_else(|| hex(v), |f| format!("f/{f}")),
    }
}

/// Shutter speed label, like "1/125" or "2\"" for whole seconds.
#[must_use]
pub fn shutter_label(v: u32) -> String {
    match find(SHUTTER, v) {
        Some(s) if s.contains('/') || !s.starts_with(|c: char| c.is_ascii_digit()) => s.into(),
        Some(s) => format!("{s}\""),
        None => hex(v),
    }
}

/// Exposure compensation label, like "+1 1/3", "-2/3" or "0".
#[must_use]
pub fn exposure_comp_label(v: u32) -> String {
    let eighths = i32::from((v as u8).cast_signed());
    let sign = match eighths.signum() {
        1 => "+",
        -1 => "-",
        _ => "",
    };
    let whole = eighths.abs().div_euclid(8);
    let frac = match eighths.abs().rem_euclid(8) {
        0 => "",
        3 => "1/3",
        4 => "1/2",
        5 => "2/3",
        _ => return hex(v),
    };
    match (whole, frac) {
        (0, "") => "0".into(),
        (0, f) => format!("{sign}{f}"),
        (w, "") => format!("{sign}{w}"),
        (w, f) => format!("{sign}{w} {f}"),
    }
}

/// White balance label, like "Daylight".
#[must_use]
pub fn white_balance_label(v: u32) -> String {
    label(WHITE_BALANCE, v)
}

/// Picture style label, like "Neutral".
#[must_use]
pub fn picture_style_label(v: u32) -> String {
    label(PICTURE_STYLE, v)
}

/// Exposure mode label for `AeMode` or `AeModeDial`, like "Av" or "Portrait".
#[must_use]
pub fn ae_mode_label(v: u32) -> String {
    label(AE_MODE, v)
}

/// Whether an exposure mode lets the app set aperture, shutter and ISO.
#[must_use]
pub const fn is_creative_mode(v: u32) -> bool {
    v <= 0x05
}

/// Live view AF method label, like "Live".
#[must_use]
pub fn af_method_label(v: u32) -> String {
    label(AF_METHOD, v)
}

/// Battery level label, like "75%".
#[must_use]
pub fn battery_label(v: u32) -> String {
    label(BATTERY, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_match_gphoto_tables() {
        assert_eq!(iso_label(0x48), "100");
        assert_eq!(iso_label(0x68), "1600");
        assert_eq!(iso_label(0x78), "6400");
        assert_eq!(iso_label(0), "Auto");
        assert_eq!(iso_label(0x49), "0x49");
        assert_eq!(aperture_label(0x30), "f/5.6");
        assert_eq!(aperture_label(0x20), "f/2.8");
        assert_eq!(aperture_label(0xffff), "Auto");
        assert_eq!(shutter_label(0x70), "1/125");
        assert_eq!(shutter_label(0x30), "2\"");
        assert_eq!(shutter_label(0x40), "0.5\"");
        assert_eq!(shutter_label(0x0c), "Bulb");
        assert_eq!(white_balance_label(9), "Colour Temperature");
        assert_eq!(picture_style_label(0x84), "Neutral");
        assert_eq!(ae_mode_label(2), "Av");
        assert_eq!(ae_mode_label(0x0c), "Portrait");
        assert_eq!(af_method_label(1), "Live");
        assert_eq!(battery_label(4), "75%");
        assert!(is_creative_mode(3) && !is_creative_mode(0x09));
    }

    #[test]
    fn exposure_comp_reads_signed_eighths() {
        assert_eq!(exposure_comp_label(0x00), "0");
        assert_eq!(exposure_comp_label(0x03), "+1/3");
        assert_eq!(exposure_comp_label(0x08), "+1");
        assert_eq!(exposure_comp_label(0x0b), "+1 1/3");
        assert_eq!(exposure_comp_label(0x18), "+3");
        assert_eq!(exposure_comp_label(0xfd), "-1/3");
        assert_eq!(exposure_comp_label(0xf8), "-1");
        assert_eq!(exposure_comp_label(0xf5), "-1 1/3");
        assert_eq!(exposure_comp_label(0xe8), "-3");
        assert_eq!(exposure_comp_label(0x01), "0x01");
    }
}
