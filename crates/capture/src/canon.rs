//! The `canon` crate's EOS camera as a [`Source`].

use canon::{Camera, Frame};

use crate::{Error, Format, Result, Source};

impl From<canon::Error> for Error {
    fn from(e: canon::Error) -> Self {
        match e {
            canon::Error::NotFound => Self::NotFound,
            canon::Error::Disconnected => Self::Disconnected,
            other => Self::Failed(other.to_string()),
        }
    }
}

impl Source for Camera {
    fn model(&self) -> &str {
        Self::model(self)
    }

    fn start(&mut self) -> Result<()> {
        Ok(self.start_live_view()?)
    }

    fn frame(&mut self, out: &mut Vec<u8>) -> Result<Option<Format>> {
        Ok(match Self::frame(self, out)? {
            Frame::Jpeg => Some(Format::Jpeg),
            Frame::NotReady => None,
        })
    }

    fn close(self: Box<Self>) -> Result<()> {
        Ok(Self::close(*self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canon_errors_keep_their_meaning() {
        assert_eq!(Error::from(canon::Error::NotFound), Error::NotFound);
        assert_eq!(Error::from(canon::Error::Disconnected), Error::Disconnected);
        assert_eq!(
            Error::from(canon::Error::OutOfRange),
            Error::Failed("argument out of range".into())
        );
    }

    #[test]
    fn a_canon_camera_is_a_source() {
        fn takes(_: Option<Box<dyn Source>>) {}
        let camera: Option<Box<Camera>> = None;
        takes(camera.map(|c| -> Box<dyn Source> { c }));
    }
}
