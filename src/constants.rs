cfg_if::cfg_if! {
    if #[cfg(target_os="linux")] {
        pub const USER_NAME: &str = "garnd";
        pub const GROUP_NAME: &str = "garnd";
        pub const SHM_FILE_NAME: &str = "shm";
        
        // Markers used to differentiate a shutdown from a reload when reading from the shutdown
        // eventfd inside the welcome thread
        pub const SHUTDOWN_EVENT: u64 = 1;
        pub const RELOAD_EVENT: u64 = 2;
    }
}

pub const WORKING_DIR_ENV_OPTION: &str = "GARND_WORKING_DIR";
