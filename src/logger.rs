use crate::constants;
use chrono::Local;
use garnshared::error_types::SendableError;
use std::fs::File;
use std::io::Write;

pub struct Logger {
    log_file: File,
}

impl Logger {
    pub fn new(log_file: File) -> Logger {
        Logger { log_file }
    }

    pub fn log_file_name() -> String {
        constants::error_log_file_name!(Local::now().format("%Y-%m-%d_%H:%M:%S%.3f").to_string())
    }

    pub fn log(&mut self, error: SendableError) {
        let datetime = Local::now().format("%Y-%m-%d %H:%M:%S%.6f").to_string();
        write!(self.log_file, "[{datetime}] {error}\n").unwrap_or_else(|e| eprintln!("{}", e));
    }
}
