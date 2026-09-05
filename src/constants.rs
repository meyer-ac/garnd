cfg_if::cfg_if! {
    if #[cfg(target_os="linux")] {
        pub const USER_NAME: &str = "garnd";
        pub const GROUP_NAME: &str = "garnd";
        pub const SHM_FILE_NAME: &str = "shm";
        
        #[macro_export]
        macro_rules! error_log_file_name {
            ($datetime:expr) => {format!("errors_{}.log", $datetime)};
        }
        pub use error_log_file_name;
    }
}

pub const WORKING_DIR_ENV_OPTION: &str = "GARND_WORKING_DIR";
