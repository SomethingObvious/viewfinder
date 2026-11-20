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
