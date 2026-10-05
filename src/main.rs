#![warn(
    clippy::all,
    //clippy::restriction,
    clippy::pedantic,
    //clippy::nursery,
    //clippy::cargo
)]

use std::process::ExitCode;
use crate::constants::{EXIT_CODE_FAILURE, EXIT_CODE_SUCCESS};
use crate::shutdown_signal::{ReloadRequest, ShutdownSignal};
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
        use linux::logger::Logger;
        use linux::util::error_in_brittle_scenario;
        use linux::circuit_breaker::circuit_breaker;
    }
}

fn main() -> ExitCode {
    let mut reload_requested ;
    loop {
        reload_requested = false;
        let mut logger = Logger::new();
        
        let (runtime, error_rx) = Runtime::new(
            get_optional_env_var(constants::WORKING_DIR_ENV_OPTION).as_deref(), // Custom working directory
        );

        if let Err(e) = runtime.notify_system_starting() {
            logger.log(&e);
            return ExitCode::from(EXIT_CODE_FAILURE);
        }

        let runtime = match runtime.init() {
            Ok(res) => res,
            Err(e) => {
                logger.log(&e);
                return ExitCode::from(EXIT_CODE_FAILURE);
            },
        };

        let runtime = match runtime.listen() {
            Ok(res) => res,
            Err(e) => {
                logger.log(&e);
                return ExitCode::from(EXIT_CODE_FAILURE);
            },
        };

        if let Err(e) = runtime.notify_system_listening() {
            logger.log(&e);
            return ExitCode::from(EXIT_CODE_FAILURE);
        }

        while let Ok(err) = error_rx.recv() {
            let mut shutdown = false;
            if err.error().is::<ShutdownSignal>() {
                if let Err(e) = runtime.notify_system_stopping() {
                    logger.log(&e);
                    // no return here, daemon is shutting down anyway
                }
                shutdown = true;
            } else if err.error().is::<ReloadRequest>() {
                circuit_breaker();
                if let Err(e) = runtime.notify_system_reloading() {
                    logger.log(&e);
                    // no return here, rather let the daemon survive and start fresh
                }
                reload_requested = true;
            }
            logger.log(&err);
            if shutdown || reload_requested {
                break;
            }
        }

        drop(runtime);

        // Catch the last few errors that may have occurred after the shutdown signal
        for err in error_rx.try_iter() {
            logger.log(&err);
        }

        if !reload_requested {
            break;
        }
    }

    ExitCode::from(EXIT_CODE_SUCCESS)
}
