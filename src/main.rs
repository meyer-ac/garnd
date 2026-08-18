#![warn(
    clippy::all,
    //clippy::restriction,
    clippy::pedantic,
    //clippy::nursery,
    //clippy::cargo
)]

use std::thread;
use std::time::Duration;
use crate::early_failure::early_failure;
use crate::util::get_optional_env_var;

mod constants;
mod early_failure;
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

    let runtime = runtime.listen();

    while let Ok(err) = error_receiver.recv() {
        eprintln!("{}", err);
    }

    //thread::sleep(Duration::from_secs(10));
}
