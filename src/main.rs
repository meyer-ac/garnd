#![warn(
    clippy::all,
    //clippy::restriction,
    clippy::pedantic,
    //clippy::nursery,
    //clippy::cargo
)]

use std::any::{Any, TypeId};
use crate::early_failure::early_failure;
use crate::shutdown_signal::ShutdownSignal;
use crate::util::get_optional_env_var;

mod constants;
mod early_failure;
mod util;
mod shutdown_signal;
mod join_guard;

cfg_if::cfg_if! {
    if #[cfg(target_os="linux")] {
        mod linux;
        use linux::runtime::Runtime;
    }
}

fn main() {
    let (runtime, error_receiver) = Runtime::new(
        get_optional_env_var(constants::WORKING_DIR_ENV_OPTION).as_deref(), // Custom working directory
    );

    let runtime = match runtime.init() {
        Ok(res) => res,
        Err(e) => early_failure(&e.to_string()),
    };

    let runtime = runtime.listen().unwrap();

    while let Ok(err) = error_receiver.recv() {
        if err.is::<ShutdownSignal>() {
            break;
        }
        eprintln!("{}", err);
    }
}
