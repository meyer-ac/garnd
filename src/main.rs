use std::env;
use crate::util::get_optional_env_var;

#[warn(
    clippy::all,
    //clippy::restriction,
    clippy::pedantic,
    //clippy::nursery,
    //clippy::cargo
)]

mod constants;
mod early_failure;
mod util;

cfg_if::cfg_if! {
    if #[cfg(target_os="linux")] {
        mod linux;
        use linux::state::State;
    }
}

fn main() {
    let state = State::new(
        get_optional_env_var(constants::WORKING_DIR_ENV_OPTION).as_deref(), // Custom working directory
    );

    println!("{}", state.working_dir());
}
