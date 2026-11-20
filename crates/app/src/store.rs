//! Settings files in `%APPDATA%\Viewfinder`.

use std::path::PathBuf;

pub(crate) fn path(name: &str) -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Viewfinder").join(name))
}

pub(crate) fn read(name: &str) -> Option<String> {
    std::fs::read_to_string(path(name)?).ok()
}

pub(crate) fn write(name: &str, text: &str) -> bool {
    path(name).is_some_and(|path| write_at(&path, text))
}

/// Writes beside the file and swaps it in, so a crash mid-write can't leave half a file.
pub(crate) fn write_at(path: &std::path::Path, text: &str) -> bool {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let part = path.with_extension("part");
    std::fs::write(&part, text).is_ok() && std::fs::rename(&part, path).is_ok()
}

pub(crate) fn numbers(text: &str) -> impl Iterator<Item = (&str, f32)> {
    text.lines().filter_map(|line| {
        let (key, value) = line.split_once('=')?;
        let value: f32 = value.trim().parse().ok()?;
        value.is_finite().then_some((key.trim(), value))
    })
}
