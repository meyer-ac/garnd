use crate::early_failure::early_failure;
use std::env;

pub fn get_optional_env_var(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(s) => Some(s),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => early_failure(
            format!("environment variable '{name}' is not a valid UTF8-string").as_str(),
        ),
    }
}

/// Document errors occurring during the destruction of an object after the usual error propagation
/// mechanisms have already been closed down or in other scenarios where propagating the error
/// normally is not possible for some reason.
///
/// This is a "quick and dirty" way to report an error that should only ever be used in that very
/// specific scenario. Its only purpose is to still document the error in some way and continue
/// with the safe deconstruction of the object (or whatever else is going on).
pub fn error_in_brittle_scenario(msg: &str) {
    eprintln!("ERROR DURING DECONSTRUCTION! {msg}");
}

#[macro_export]
macro_rules! send_boxed_error {
    ($error_tx:expr, $err:expr) => {
        if $error_tx.send($err).is_err() {
            panic!("Error propagation channel broke down unexpectedly.")
        }
    };
}

#[macro_export]
macro_rules! send_error {
    ($error_tx:expr, $err:expr) => {
        $crate::send_boxed_error!($error_tx, Box::new($err))
    };
}

#[macro_export]
macro_rules! send_all_errors {
    ($error_tx:expr, $errs:expr) => {
        for err in $errs {
            $crate::send_boxed_error!($error_tx, err)
        }
    };
}
