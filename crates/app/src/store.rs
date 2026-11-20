//! Settings files in `%APPDATA%\Viewfinder`.

use std::path::PathBuf;

pub(crate) fn path(name: &str) -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Viewfinder").join(name))
}

/// The file's text, or None if it's missing.
pub(crate) fn read(name: &str) -> Option<String> {
    std::fs::read_to_string(path(name)?).ok()
}

/// Makes the folder if it's missing and writes the file. True if it was written.
pub(crate) fn write(name: &str, text: &str) -> bool {
    let Some(path) = path(name) else { return false };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(path, text).is_ok()
}

/// Each `key=value` line whose value is a number.
pub(crate) fn numbers(text: &str) -> impl Iterator<Item = (&str, f32)> {
    text.lines().filter_map(|line| {
        let (key, value) = line.split_once('=')?;
        Some((key.trim(), value.trim().parse().ok()?))
    })
}
