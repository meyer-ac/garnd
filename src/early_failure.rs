use std::process::abort;

pub fn early_failure(message: &str) -> !{
    panic!("garnd: {message}");
    abort() // If the unwind is caught for some reason.
}