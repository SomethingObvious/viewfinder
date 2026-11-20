//! The camera to use and the model the virtual camera is named after.

use capture::Choice;

use crate::store;

fn read() -> String {
    store::read("camera.txt").unwrap_or_default()
}

/// Older files hold a bare name on the first line.
fn model_in(text: &str) -> &str {
    text.lines()
        .find_map(|l| l.strip_prefix("model="))
        .or_else(|| text.lines().next().filter(|l| !l.contains('=')))
        .unwrap_or("")
        .trim()
}

/// What camera.txt can hold of a model's name. It comes from the camera, so a line break in it
/// can't add lines of its own.
fn clean(model: &str) -> String {
    model
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

fn text(model: &str, choice: &Choice) -> String {
    format!("model={}\n{}", clean(model), choice.lines())
}

fn write(model: &str, choice: &Choice) -> bool {
    store::write("camera.txt", &text(model, choice))
}

pub(crate) fn load() -> Choice {
    Choice::read(&read())
}

pub(crate) fn save(choice: &Choice) {
    write(model_in(&read()), choice);
}

pub(crate) fn model() -> String {
    model_in(&read()).to_owned()
}

/// True if the file changed.
pub(crate) fn save_model(model: &str) -> bool {
    let old = read();
    model_in(&old) != clean(model) && write(model, &Choice::read(&old))
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
