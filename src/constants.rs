cfg_if::cfg_if! {
    if #[cfg(target_os="linux")] {
        pub const USER_NAME: &str = "garnd";
        pub const GROUP_NAME: &str = "garnd";
        pub const SHM_FILE_NAME: &str = "shm";
    }
}

pub const WORKING_DIR_ENV_OPTION: &str = "GARND_WORKING_DIR";