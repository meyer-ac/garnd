use std::error::Error;
use std::fmt::{Debug, Display, Formatter};

#[derive(Debug)]
pub struct ShutdownSignal {}

impl Display for ShutdownSignal {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("Service shutdown requested.")
    }
}

impl Error for ShutdownSignal {}
