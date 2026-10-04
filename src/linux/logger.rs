use garnshared::error_types::SendableErrorWithMetadata;

// For daemons like this one primarily intended to run either as a service or in a container, it is
// best practice to just print error messages to stderr.

pub struct Logger {}

impl Logger {
    pub fn new() -> Logger {
        Logger {}
    }

    #[allow(clippy::unused_self)]
    pub fn log(&mut self, error: &SendableErrorWithMetadata) {
        eprintln!("{error}");
    }
}
