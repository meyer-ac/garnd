use std::mem::ManuallyDrop;
use std::thread;
use std::thread::JoinHandle;
use crate::util::warn;

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
        let result = unsafe {ManuallyDrop::<JoinHandle<()>>::take(&mut self.join_handle)}.join();
        if result.is_err() {
            if thread::panicking() {
                warn(format!("Joining thread in the destructor of JoinGuard failed: {:?}", result.unwrap_err()).as_str());
            } else {
                result.unwrap();
            }
        }
    }
}