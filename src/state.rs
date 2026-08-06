use crate::constants;

struct State {
    pub run_dir: String,
    pub welcome_sock_name: String,
}

impl State {
    fn new() -> Self {
        Self {
            run_dir: constants::RUN_DIR.to_string(),
            welcome_sock_name: constants::WELCOME_SOCK_NAME.to_string(),
        }
    }
}