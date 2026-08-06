use std::env;
use crate::constants;
use crate::early_failure::early_failure;

pub fn get_optional_env_var(name: &str) -> Option<String> {
    match env::var(constants::WORKING_DIR_ENV_OPTION) {
        Ok(s) => Some(s),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => early_failure(format!("Environment variable '{}' is not a valid UTF8-string.", name).as_str())
    }
}