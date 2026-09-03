use crate::util::{try_extract_error_message, warn};
use std::mem::ManuallyDrop;
use std::thread;
use std::thread::JoinHandle;

pub struct JoinGuard {
    join_handle: ManuallyDrop<JoinHandle<()>>,
}

impl From<JoinHandle<()>> for JoinGuard {
    fn from(join_handle: JoinHandle<()>) -> Self {
        Self {
            join_handle: ManuallyDrop::new(join_handle),
        }
    }
}

impl Drop for JoinGuard {
    fn drop(&mut self) {
        let result = unsafe { ManuallyDrop::<JoinHandle<()>>::take(&mut self.join_handle) }.join();
        if let Err(e) = &result {
            if thread::panicking() {
                warn(
                    format!(
                        "Joining thread in the destructor of JoinGuard failed: {}",
                        try_extract_error_message(e)
                    )
                    .as_str(),
                );
            } else {
                #[allow(clippy::panicking_unwrap)] // panic is intended here
                result.unwrap();
            }
        }
    }
}
