use crate::linux::envoironment_thread::environment_thread_main;
use garnshared::error_types::SendableError;
use nix::sys::eventfd::{EfdFlags, EventFd};
use std::mem::ManuallyDrop;
use std::os::fd::OwnedFd;
use std::panic::resume_unwind;
use std::sync::mpsc::Sender;
use std::sync::{Arc, mpsc};
use std::thread;
use std::thread::JoinHandle;
use nix::errno::Errno;
use crate::util::warn;

pub struct Environment {
    add_listener_event: Arc<EventFd>,
    add_listener_tx: Sender<OwnedFd>,
    drop_event: Arc<EventFd>,
    thread: JoinHandle<()>,
}

impl Environment {
    pub fn new(
        name: &str,
        socket: OwnedFd,
        error_tx: Sender<SendableError>,
        close_env_event: Arc<EventFd>,
        close_env_tx: Sender<String>,
    ) -> Result<Self, SendableError> {
        let name = name.to_owned();
        let add_listener_event = Arc::new(
            EventFd::from_value_and_flags(0, EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
                ?,
        );
        let (add_listener_tx, add_listener_rx) = mpsc::channel::<OwnedFd>();
        let drop_event = Arc::new(
            EventFd::from_value_and_flags(0, EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
                ?,
        );
        let thread_add_listener_event = add_listener_event.clone();
        let thread_drop_event = drop_event.clone();
        let thread = thread::spawn(move || {
            environment_thread_main(
                &name,
                error_tx,
                close_env_event,
                close_env_tx,
                thread_add_listener_event,
                add_listener_rx,
                thread_drop_event,
            );
        });
        let mut self_ = Self {
            add_listener_event,
            add_listener_tx,
            drop_event,
            thread,
        };
        self_.insert_socket(socket)?;
        Ok(self_)
    }

    pub fn insert_socket(&mut self, socket: OwnedFd) -> Result<(), SendableError> {
        self.add_listener_tx.send(socket)?;
        self.add_listener_event.write(1)?;
        Ok(())
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        if let Err(e) = self.drop_event.write(1) {
            if thread::panicking() {
                warn(&format!("Environment panicked while destructing: {}", e.to_string()));
            } else {
                Err::<usize, Errno>(e).unwrap();
            }
        }
    }
}
