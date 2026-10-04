use crate::join_guard::JoinGuard;
use crate::linux::environment_synchronization_thread::environment_synchronization_thread_main;
use crate::util::error_in_brittle_scenario;
use garnshared::error_types::{ResultMetadata, DetailedError};
use nix::sys::eventfd::{EfdFlags, EventFd};
use std::os::fd::OwnedFd;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, mpsc};
use std::thread;

pub struct Environment {
    add_listener_event: Arc<EventFd>,
    add_listener_tx: Sender<OwnedFd>,
    sync_response_rx: Receiver<Result<(), DetailedError>>,
    drop_event: Arc<EventFd>,
    _thread: JoinGuard,
}

impl Environment {
    pub fn new(
        name: &str,
        error_tx: Sender<DetailedError>,
        close_env_event: Arc<EventFd>,
        close_env_tx: Sender<String>,
    ) -> Result<Self, DetailedError> {
        let name = name.to_owned();
        let add_listener_event = Arc::new(EventFd::from_value_and_flags(
            0,
            EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK,
        ).add_metadata()?);
        let (add_listener_tx, add_listener_rx) = mpsc::channel::<OwnedFd>();
        let (sync_response_tx, sync_response_rx) = mpsc::channel::<Result<(), DetailedError>>();
        let drop_event = Arc::new(EventFd::from_value_and_flags(
            0,
            EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK,
        ).add_metadata()?);
        let thread_add_listener_event = add_listener_event.clone();
        let thread_drop_event = drop_event.clone();
        let thread = JoinGuard::from(thread::Builder::new().spawn(move || {
            environment_synchronization_thread_main(
                &name,
                &error_tx,
                &sync_response_tx,
                &close_env_event,
                &close_env_tx,
                &thread_add_listener_event,
                &add_listener_rx,
                &thread_drop_event,
            );
        }).add_metadata()?);
        Ok(Self {
            add_listener_event,
            add_listener_tx,
            sync_response_rx,
            drop_event,
            _thread: thread,
        })
    }

    pub fn insert_socket(&mut self, socket: OwnedFd) -> Result<(), DetailedError> {
        self.add_listener_tx.send(socket).add_metadata()?;
        self.add_listener_event.write(1).add_metadata()?;
        self.sync_response_rx.recv().add_metadata()?
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        let result = self.drop_event.write(1);
        if let Err(e) = &result {
            if thread::panicking() {
                error_in_brittle_scenario(&format!("Environment panicked while destructing: {e}"));
            } else {
                result.unwrap();
            }
        }
    }
}
