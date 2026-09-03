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

pub fn warn(msg: &str) {
    eprintln!("garnd: WARNING! {msg}");
}

pub fn try_extract_error_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .copied()
        .map(str::to_owned)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or(format!("{payload:?}"))
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
