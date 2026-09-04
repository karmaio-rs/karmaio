use std::io;

use crate::runtime::{is_operation_canceled, operation_canceled};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Failure {
    kind: io::ErrorKind,
    canceled: bool,
}

impl Failure {
    pub(crate) const fn abandoned_io() -> Self {
        Self {
            kind: io::ErrorKind::Other,
            canceled: false,
        }
    }

    pub(crate) fn from_error(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            canceled: is_operation_canceled(error),
        }
    }

    pub(crate) fn error(self) -> io::Error {
        if self.canceled {
            operation_canceled()
        } else {
            io::Error::new(self.kind, "TLS stream is unusable after a previous error")
        }
    }
}
