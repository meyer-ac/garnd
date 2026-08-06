use crate::constants;
use crate::linux::global_listener::GlobalListener;

pub struct State {
    working_dir: String,
    welcome_sock_name: String,
    global_listener: GlobalListener,
}

impl State {
    pub fn new(working_dir: Option<&str>) -> Self {
        let working_dir = working_dir.unwrap_or(constants::WORKING_DIR).to_string();
        let welcome_sock_name = constants::WELCOME_SOCK_NAME.to_string();
        let global_listener = GlobalListener::new(working_dir.as_str(), welcome_sock_name.as_str());

        Self {
            working_dir,
            welcome_sock_name,
            global_listener,
        }
    }

    pub fn working_dir(&self) -> &str {
        self.working_dir.as_str()
    }

    pub fn global_listener(&self) -> &GlobalListener {
        &self.global_listener
    }
}