cfg_if::cfg_if! {
    if #[cfg(target_os="linux")] {
        pub const USER_NAME: &str = "garnd";
        pub const GROUP_NAME: &str = "garnd";
        pub const SHM_FILE_NAME: &str = "shm";
        
        /// Generates a filename for the error log based on the provided time.
        /// Nondeterministic by design to ensure unique filenames.
        #[macro_export]
        macro_rules! error_log_file_name {
            ($datetime:expr) => {format!("{}_garnd_errors_{}.log", $datetime.format("%Y-%m-%dT%H:%M:%S%.f%:z").to_string(), ::uuid::Uuid::new_v4())};
        }
        pub use error_log_file_name;
    }
}

pub const WORKING_DIR_ENV_OPTION: &str = "GARND_WORKING_DIR";
