#![warn(
    clippy::all,
    //clippy::restriction,
    clippy::pedantic,
    //clippy::nursery,
    //clippy::cargo
)]

use crate::early_failure::early_failure;
use crate::shutdown_signal::ShutdownSignal;
use crate::util::get_optional_env_var;

mod constants;
mod early_failure;
mod join_guard;
mod shutdown_signal;
mod util;

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

    let _runtime = runtime
        .listen()
        .unwrap_or_else(|e| early_failure(&e.to_string()));

    while let Ok(err) = error_receiver.recv() {
        eprintln!("{err}");
        if err.is::<ShutdownSignal>() {
            break;
        }
    }

    // Catch the last few errors that may have occurred after the shutdown signal
    for err in error_receiver.try_iter() {
        eprintln!("{err}");
    }
}
