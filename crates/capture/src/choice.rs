//! Which camera Viewfinder uses, kept as `key=value` lines like the other settings files.

/// A camera maker whose bodies stream over PTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    /// Canon EOS, through the `canon` crate.
    Canon,
    /// Nikon, see [`crate::nikon`].
    Nikon,
    /// Sony, see [`crate::sony`].
    Sony,
}

impl Vendor {
    const ALL: [Self; 3] = [Self::Canon, Self::Nikon, Self::Sony];

    /// The USB vendor id as it appears in a device path.
    #[must_use]
    pub const fn vid(self) -> &'static str {
        match self {
            Self::Canon => "vid_04a9",
            Self::Nikon => "vid_04b0",
            Self::Sony => "vid_054c",
        }
    }

    /// The maker whose USB vendor id is in `path`.
    #[must_use]
    pub fn of(path: &str) -> Option<Self> {
        let path = path.to_ascii_lowercase();
        Self::ALL.into_iter().find(|v| path.contains(v.vid()))
    }

    const fn key(self) -> &'static str {
        match self {
            Self::Canon => "canon",
            Self::Nikon => "nikon",
            Self::Sony => "sony",
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Canon => "Canon EOS",
            Self::Nikon => "Nikon",
            Self::Sony => "Sony",
        }
    }
}

/// One camera, named so the same one is found again after it's unplugged or Viewfinder restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// A body on PTP, by the name Windows gives it. An empty `model` takes any body from
    /// `vendor`.
    Ptp {
        /// The maker.
        vendor: Vendor,
        /// The body's name in Windows, such as "Canon EOS REBEL T3".
        model: String,
    },
    /// A webcam or capture card, by its symbolic link, or by `name` when the link has changed,
    /// as it does when the device goes into another USB port.
    Video {
        /// The device interface path Media Foundation opens it by.
        link: String,
        /// Its friendly name.
        name: String,
    },
}

impl Default for Choice {
    fn default() -> Self {
        Self::Ptp {
            vendor: Vendor::Canon,
            model: String::new(),
        }
    }
}

impl Choice {
    /// Reads the choice from a settings file's lines. Anything missing or unknown gives the
    /// Canon.
    #[must_use]
    pub fn read(text: &str) -> Self {
        let (mut kind, mut device, mut link) = ("", "", "");
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "camera" => kind = value.trim(),
                "device" => device = value.trim(),
                "link" => link = value.trim(),
                _ => {}
            }
        }
        if kind == "webcam" && !(link.is_empty() && device.is_empty()) {
            return Self::Video {
                link: link.into(),
                name: device.into(),
            };
        }
        Vendor::ALL
            .into_iter()
            .find(|v| v.key() == kind)
            .map_or_else(Self::default, |vendor| Self::Ptp {
                vendor,
                model: device.into(),
            })
    }

    /// The lines [`Choice::read`] reads back.
    #[must_use]
    pub fn lines(&self) -> String {
        match self {
            Self::Ptp { vendor, model } => format!("camera={}\ndevice={model}\n", vendor.key()),
            Self::Video { link, name } => format!("camera=webcam\ndevice={name}\nlink={link}\n"),
        }
    }

    /// The name to show for it.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Ptp { vendor, model } if model.is_empty() => vendor.name(),
            Self::Ptp { model: name, .. } | Self::Video { name, .. } => name,
        }
    }

    /// Where this camera is among those plugged in. A webcam's link wins over its name, which
    /// tells apart two webcams with the same name.
    #[must_use]
    pub fn find(&self, found: &[Self]) -> Option<usize> {
        match self {
            Self::Ptp { vendor, model } => found.iter().position(|c| {
                matches!(c, Self::Ptp { vendor: v, model: m }
                    if v == vendor && (model.is_empty() || m == model))
            }),
            Self::Video { link, name } => found
                .iter()
                .position(
                    |c| matches!(c, Self::Video { link: l, .. } if l.eq_ignore_ascii_case(link)),
                )
                .or_else(|| {
                    found.iter().position(|c| {
                        matches!(c, Self::Video { name: n, .. } if !name.is_empty() && n == name)
                    })
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ptp(vendor: Vendor, model: &str) -> Choice {
        Choice::Ptp {
            vendor,
            model: model.into(),
        }
    }

    fn video(link: &str, name: &str) -> Choice {
        Choice::Video {
            link: link.into(),
            name: name.into(),
        }
    }

    #[test]
    fn no_file_means_the_canon() {
        assert_eq!(Choice::read(""), Choice::default());
        assert_eq!(Choice::read("Canon EOS REBEL T3\n"), Choice::default());
        assert_eq!(Choice::read("camera=polaroid\n"), Choice::default());
        assert_eq!(Choice::read("camera=webcam\n"), Choice::default());
        assert_eq!(Choice::default().label(), "Canon EOS");
    }

    #[test]
    fn reads_back_what_it_writes() {
        let link =
            r"\\?\usb#vid_30c9&pid_00ac&mi_00#6&1#{e5323777-f976-4f5b-9b55-b94699c46e44}\global";
        for c in [
            Choice::default(),
            ptp(Vendor::Nikon, "D3300"),
            ptp(Vendor::Sony, "ILCE-6400"),
            video(link, "Integrated Camera"),
        ] {
            let text = format!("model=Canon EOS REBEL T3\n{}", c.lines());
            assert_eq!(Choice::read(&text), c);
        }
    }

    #[test]
    fn finds_by_name_or_link() {
        let found = [
            ptp(Vendor::Canon, "Canon EOS REBEL T3"),
            ptp(Vendor::Nikon, "D3300"),
            video("link-a", "USB Camera"),
            video("LINK-B", "USB Camera"),
        ];
        let at = |c: &Choice| {
            c.find(&found)
                .and_then(|i| found.get(i))
                .map(|f| f.label().to_owned())
        };
        assert_eq!(
            at(&Choice::default()).as_deref(),
            Some("Canon EOS REBEL T3")
        );
        assert_eq!(at(&ptp(Vendor::Nikon, "D3300")).as_deref(), Some("D3300"));
        assert_eq!(at(&ptp(Vendor::Nikon, "D750")), None);
        assert_eq!(at(&ptp(Vendor::Sony, "")), None);
        assert_eq!(
            video("link-b", "USB Camera").find(&found),
            Some(3),
            "the link picks out one of two webcams with the same name"
        );
        assert_eq!(
            video("moved", "USB Camera").find(&found),
            Some(2),
            "a moved webcam is found by its name"
        );
        assert_eq!(video("moved", "").find(&found), None);
    }

    #[test]
    fn vendors_come_from_device_paths() {
        assert_eq!(
            Vendor::of(r"\\?\USB#VID_04A9&PID_3217#3#{6ac27878-a6fa-4155-ba85-f98f491d4f33}"),
            Some(Vendor::Canon)
        );
        assert_eq!(Vendor::of(r"\\?\usb#vid_054c&pid_0994"), Some(Vendor::Sony));
        assert_eq!(Vendor::of(r"\\?\usb#vid_18d1&pid_4ee1"), None);
    }
}
