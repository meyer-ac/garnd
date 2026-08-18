use std::env;
use crate::constants;
use crate::early_failure::early_failure;

pub fn get_optional_env_var(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(s) => Some(s),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => early_failure(format!("environment variable '{name}' is not a valid UTF8-string").as_str())
    }
}

pub fn warn(msg: &str) {
    eprintln!("garnd: WARNING! {msg}");
}

pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<?>")
}