//! The camera to use, kept in `%APPDATA%\Viewfinder\camera.txt` with the model name the
//! virtual camera is called after.

use capture::Choice;

use crate::store;

fn read() -> String {
    store::read("camera.txt").unwrap_or_default()
}

/// The `model` line, or a bare name on the first line as older files hold.
fn model_in(text: &str) -> &str {
    text.lines()
        .find_map(|l| l.strip_prefix("model="))
        .or_else(|| text.lines().next().filter(|l| !l.contains('=')))
        .unwrap_or("")
        .trim()
}

fn text(model: &str, choice: &Choice) -> String {
    format!("model={model}\n{}", choice.lines())
}

fn write(model: &str, choice: &Choice) -> bool {
    store::write("camera.txt", &text(model, choice))
}

/// The chosen camera, the Canon when there's no file.
pub(crate) fn load() -> Choice {
    Choice::read(&read())
}

pub(crate) fn save(choice: &Choice) {
    write(model_in(&read()), choice);
}

/// The model of the camera last opened, or empty before one has been.
pub(crate) fn model() -> String {
    model_in(&read()).to_owned()
}

/// Saves the model of the camera just opened. True if the file changed.
pub(crate) fn save_model(model: &str) -> bool {
    let old = read();
    model_in(&old) != model && write(model, &Choice::read(&old))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_model_and_the_choice_apart() {
        assert_eq!(model_in(""), "");
        assert_eq!(model_in("Canon EOS REBEL T3\n"), "Canon EOS REBEL T3");
        assert_eq!(Choice::read("Canon EOS REBEL T3\n"), Choice::default());
        let webcam = Choice::Video {
            link: r"\?\usb#vid_30c9&pid_00ac".into(),
            name: "Integrated Camera".into(),
        };
        let t = text("Canon EOS REBEL T3", &webcam);
        assert_eq!(model_in(&t), "Canon EOS REBEL T3");
        assert_eq!(Choice::read(&t), webcam);
        assert_eq!(model_in("camera=canon\ndevice=\n"), "");
    }
}
