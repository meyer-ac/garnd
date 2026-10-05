#![warn(
    clippy::all,
    //clippy::restriction,
    clippy::pedantic,
    //clippy::nursery,
    //clippy::cargo
)]

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

fn main() -> Result<(), std::io::Error> {
    let mut reload_requested ;
    loop {
        reload_requested = false;
        let (runtime, error_tx, error_rx) = Runtime::new(
            get_optional_env_var(constants::WORKING_DIR_ENV_OPTION).as_deref(), // Custom working directory
        );

        if let Err(e) = runtime.notify_system_starting() {
            send_error!(error_tx, e);
        }

        let mut logger = Logger::new();

        let runtime = match runtime.init() {
            Ok(res) => res,
            Err(e) => {
                logger.log(&e);
                return Err(std::io::Error::from_raw_os_error(Runtime::error_return_code()));
            },
        };

        let runtime = match runtime.listen() {
            Ok(res) => res,
            Err(e) => {
                logger.log(&e);
                return Err(std::io::Error::from_raw_os_error(Runtime::error_return_code()));
            },
        };

        if let Err(e) = runtime.notify_system_listening() {
            send_error!(error_tx, e);
        }

        while let Ok(err) = error_rx.recv() {
            let mut shutdown = false;
            if err.error().is::<ShutdownSignal>() {
                if let Err(e) = runtime.notify_system_stopping() {
                    send_error!(error_tx, e);
                }
                shutdown = true;
            } else if err.error().is::<ReloadRequest>() {
                circuit_breaker();
                if let Err(e) = runtime.notify_system_reloading() {
                    send_error!(error_tx, e);
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

    Ok(())
}
