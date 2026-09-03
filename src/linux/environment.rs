use crate::join_guard::JoinGuard;
use crate::linux::environment_thread::environment_thread_main;
use crate::util::warn;
use garnshared::error_types::SendableError;
use nix::sys::eventfd::{EfdFlags, EventFd};
use std::os::fd::OwnedFd;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, mpsc};
use std::thread;

pub struct Environment {
    add_listener_event: Arc<EventFd>,
    add_listener_tx: Sender<OwnedFd>,
    sync_response_rx: Receiver<Result<(), SendableError>>,
    drop_event: Arc<EventFd>,
    _thread: JoinGuard,
}

impl Environment {
    pub fn new(
        name: &str,
        error_tx: Sender<SendableError>,
        close_env_event: Arc<EventFd>,
        close_env_tx: Sender<String>,
    ) -> Result<Self, SendableError> {
        let name = name.to_owned();
        let add_listener_event = Arc::new(EventFd::from_value_and_flags(
            0,
            EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK,
        )?);
        let (add_listener_tx, add_listener_rx) = mpsc::channel::<OwnedFd>();
        let (sync_response_tx, sync_response_rx) = mpsc::channel::<Result<(), SendableError>>();
        let drop_event = Arc::new(EventFd::from_value_and_flags(
            0,
            EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK,
        )?);
        let thread_add_listener_event = add_listener_event.clone();
        let thread_drop_event = drop_event.clone();
        let thread = JoinGuard::from(thread::Builder::new().spawn(move || {
            environment_thread_main(
                &name,
                &error_tx,
                &sync_response_tx,
                &close_env_event,
                &close_env_tx,
                &thread_add_listener_event,
                &add_listener_rx,
                &thread_drop_event,
            );
        })?);
        Ok(Self {
            add_listener_event,
            add_listener_tx,
            sync_response_rx,
            drop_event,
            _thread: thread,
        })
    }

    pub fn insert_socket(&mut self, socket: OwnedFd) -> Result<(), SendableError> {
        self.add_listener_tx.send(socket)?;
        self.add_listener_event.write(1)?;
        self.sync_response_rx.recv()?
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        let result = self.drop_event.write(1);
        if let Err(e) = &result {
            if thread::panicking() {
                warn(&format!("Environment panicked while destructing: {e}"));
            } else {
                result.unwrap();
            }
        }
    }
}
