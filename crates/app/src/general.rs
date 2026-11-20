//! The General settings, and Start with Windows, which is the Run value install.ps1 writes.

use std::time::Duration;

use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::HSTRING;

pub(crate) const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "Viewfinder";

/// What the window's Close button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Close {
    Hide,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Prefs {
    /// Minutes the window alone keeps live view up, or 0 for no limit.
    pub pause_after: u32,
    pub close: Close,
    pub on_top: bool,
    pub battery_warning: bool,
    /// Thirds lines over Preview.
    pub guide: bool,
    /// The restored window's left, top, right and bottom in desktop pixels.
    pub window: Option<[i32; 4]>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            pause_after: 3,
            close: Close::Hide,
            on_top: false,
            battery_warning: true,
            guide: false,
            window: None,
        }
    }
}

impl Prefs {
    /// How long the window alone keeps live view up, or None for as long as it's open.
    pub(crate) fn pause(self) -> Option<Duration> {
        (self.pause_after > 0).then(|| Duration::from_secs(u64::from(self.pause_after) * 60))
    }

    fn parse(text: &str) -> Self {
        let mut prefs = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            let on = value == "1";
            match key.trim() {
                "pause_after" => {
                    if let Ok(v) = value.parse() {
                        prefs.pause_after = v;
                    }
                }
                "close_quits" => prefs.close = if on { Close::Quit } else { Close::Hide },
                "on_top" => prefs.on_top = on,
                "battery_warning" => prefs.battery_warning = on,
                "guide" => prefs.guide = on,
                "window" => {
                    let sides: Vec<i32> = value.split(',').filter_map(|v| v.parse().ok()).collect();
                    prefs.window = <[i32; 4]>::try_from(sides).ok();
                }
                _ => {}
            }
        }
        prefs
    }

    fn text(self) -> String {
        let window = self
            .window
            .map(|[l, t, r, b]| format!("window={l},{t},{r},{b}\n"))
            .unwrap_or_default();
        format!(
            "pause_after={}\nclose_quits={}\non_top={}\nbattery_warning={}\nguide={}\n{window}",
            self.pause_after,
            u8::from(self.close == Close::Quit),
            u8::from(self.on_top),
            u8::from(self.battery_warning),
            u8::from(self.guide),
        )
    }
}

pub(crate) fn load() -> Prefs {
    crate::store::read("general.txt").map_or_else(Prefs::default, |t| Prefs::parse(&t))
}

pub(crate) fn save(prefs: Prefs) {
    crate::store::write("general.txt", &prefs.text());
}

/// The same command install.ps1 writes.
fn command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" --background", exe.display()))
}

#[expect(unsafe_code, reason = "reading the registry is a Win32 call")]
pub(crate) fn starts_with_windows(key: &str) -> bool {
    // SAFETY: the names are live wide strings, and no data is asked for.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            &HSTRING::from(RUN_NAME),
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
    }
    .is_ok()
}

#[expect(unsafe_code, reason = "writing the registry is a Win32 call")]
pub(crate) fn set_starts_with_windows(key: &str, on: bool) -> bool {
    let (key, name) = (HSTRING::from(key), HSTRING::from(RUN_NAME));
    if !on {
        // SAFETY: the names are live wide strings.
        let removed = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &name) };
        return removed.is_ok() || removed == ERROR_FILE_NOT_FOUND;
    }
    let Some(command) = command() else {
        return false;
    };
    let wide: Vec<u16> = command.encode_utf16().chain([0]).collect();
    // SAFETY: the data pointer and byte count describe `wide`, which outlives the call.
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &key,
            &name,
            REG_SZ.0,
            Some(wide.as_ptr().cast()),
            (wide.len() * 2) as u32,
        )
    }
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_round_trip() {
        let prefs = Prefs {
            pause_after: 0,
            close: Close::Quit,
            on_top: true,
            battery_warning: false,
            guide: true,
            window: Some([-1200, 40, -100, 700]),
        };
        assert_eq!(Prefs::parse(&prefs.text()), prefs);
        assert_eq!(Prefs::parse(""), Prefs::default());
        assert_eq!(Prefs::parse("window=1,2,3"), Prefs::default());
        assert_eq!(Prefs::default().pause(), Some(Duration::from_secs(180)));
        assert_eq!(prefs.pause(), None);
    }

    /// Against a made-up key, never the real Run key.
    #[test]
    #[expect(
        unsafe_code,
        reason = "reading and removing the test key are Win32 calls"
    )]
    fn start_with_windows_writes_and_removes_the_run_value() {
        use windows::Win32::System::Registry::RegDeleteTreeW;
        let key = format!(r"Software\Viewfinder Test {}\Run", std::process::id());
        assert!(!starts_with_windows(&key));
        assert!(set_starts_with_windows(&key, true));
        assert!(starts_with_windows(&key));
        let mut buf = [0u16; 1024];
        let mut size = size_of_val(&buf) as u32;
        // SAFETY: `size` says how many bytes `buf` holds.
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(&key),
                &HSTRING::from(RUN_NAME),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&raw mut size),
            )
        }
        .ok()
        .unwrap();
        let value = String::from_utf16_lossy(
            buf.get(..(size as usize).div_euclid(2).saturating_sub(1))
                .unwrap(),
        );
        assert_eq!(Some(value.clone()), command());
        assert!(value.ends_with(".exe\" --background"), "{value}");
        assert!(set_starts_with_windows(&key, false));
        assert!(!starts_with_windows(&key));
        assert!(set_starts_with_windows(&key, false), "already gone is fine");
        let parent = key.trim_end_matches(r"\Run");
        let parent = HSTRING::from(parent);
        // SAFETY: the name is a live wide string. This removes the made-up key and all under it.
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &parent) }
            .ok()
            .unwrap();
    }
}
