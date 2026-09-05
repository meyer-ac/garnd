#![warn(
    clippy::all,
    //clippy::restriction,
    clippy::pedantic,
    //clippy::nursery,
    //clippy::cargo
)]

use crate::early_failure::early_failure;
use crate::logger::Logger;
use crate::shutdown_signal::ShutdownSignal;
use crate::util::get_optional_env_var;

mod constants;
mod early_failure;
mod join_guard;
mod logger;
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

    let mut logger = Logger::new(
        runtime
            .create_log_file(&Logger::log_file_name())
            .unwrap_or_else(|e| early_failure(&e.to_string())),
    );

    let runtime = runtime
        .listen()
        .unwrap_or_else(|e| early_failure(&e.to_string()));

    while let Ok(err) = error_receiver.recv() {
        let mut shutdown = false;
        if err.is::<ShutdownSignal>() {
            shutdown = true;
        }
        logger.log(err);
        if shutdown {
            break;
        }
    }

    drop(runtime);

    // Catch the last few errors that may have occurred after the shutdown signal
    for err in error_receiver.try_iter() {
        logger.log(err);
    }
}
