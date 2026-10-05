use std::process::exit;
use crate::constants::EXIT_CODE_FAILURE;

pub fn early_failure(message: &str) -> ! {
    eprintln!("EARLY FAILURE! {message}");
    exit(i32::from(EXIT_CODE_FAILURE));
}
