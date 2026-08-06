cfg_if::cfg_if! {
    if #[cfg(target_os="linux")] {
        pub const WORKING_DIR: &str = "/run/garnd";
        pub const WELCOME_SOCK_NAME: &str = "welcome.sock";
    }
}

pub const WORKING_DIR_ENV_OPTION: &str = "GARND_WORKING_DIR";