use crate::linux::environment::Environment;
use crate::linux::runtime_error::RuntimeError;
use garnshared::constants::{MAX_NAME_LEN, WELCOME_REQUEST_SIZE};
use garnshared::welcome_protocol::{WelcomeRequest, WelcomeResponse};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::socket::{Backlog, MsgFlags, accept, listen, recv, send};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::error::Error;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::{mpsc, Arc};
use std::sync::mpsc::Sender;
use nix::unistd::{pipe, read};
use nix::errno::Errno;

pub fn welcome_thread_main(
    error_tx: Sender<Box<dyn Error + Send>>,
    welcome_socket: OwnedFd,
    shutdown_event: Arc<EventFd>,
) -> Option<OwnedFd> {
    let mut environments = HashMap::new();
    let close_env_event = Arc::new(match EventFd::from_value_and_flags(
        0,
        EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK,
    ) {
        Ok(res) => res,
        Err(e) => {
            error_tx.send(Box::new(e)).unwrap();
            return None;
        }
    });
    let (close_env_tx, close_env_rx) = mpsc::channel::<String>();

    // todo: proper error handling

    if let Err(e) = listen(&welcome_socket.as_fd(), Backlog::MAXCONN) {
        error_tx.send(Box::new(e)).unwrap();
        return None;
    }

    loop {
        let poll_fds = &mut [
            PollFd::new(shutdown_event.as_fd(), PollFlags::POLLIN),
            PollFd::new(close_env_event.as_fd(), PollFlags::POLLIN),
            PollFd::new(welcome_socket.as_fd(), PollFlags::POLLIN),
        ];

        if let Err(e) = poll(poll_fds, PollTimeout::NONE) {
            error_tx.send(Box::new(e)).unwrap();
            return None;
        }

        if poll_fds[0].any().unwrap_or_default() {
            return Some(welcome_socket);
        }

        if poll_fds[1].any().unwrap_or_default() {
            let name = match close_env_rx.recv() {
                Ok(res) => res,
                Err(e) => {
                    error_tx.send(Box::new(e)).unwrap();
                    return Some(welcome_socket);
                }
            };
            environments.remove(&name);
        }

        if !poll_fds[2].revents().unwrap().contains(PollFlags::POLLIN) {
            error_tx
                .send(Box::new(RuntimeError::WelcomeSocketFailed))
                .unwrap();
            return None;
        }

        let client_fd = match accept(welcome_socket.as_raw_fd()) {
            // SAFETY: res is open and suitable for taking ownership; the raw fd is immediately discarded
            Ok(res) => unsafe {OwnedFd::from_raw_fd(res)},
            Err(e) => {
                error_tx.send(Box::new(e)).unwrap();
                continue;
            }
        };
        let raw_fd = client_fd.as_raw_fd();

        let mut buffer: [u8; WELCOME_REQUEST_SIZE] = [0; WELCOME_REQUEST_SIZE];
        if let Err(e) = recv(raw_fd, &mut buffer, MsgFlags::empty()) {
            error_tx.send(Box::new(e)).unwrap();
            continue;
        }

        let Ok(request_str) = String::from_utf8(buffer.to_vec()) else {
            let response = WelcomeResponse::MalformedRequest.serialize();
            if let Err(e) = send(raw_fd, response.as_bytes(), MsgFlags::empty()) {
                error_tx.send(Box::new(e)).unwrap();
            }
            continue;
        };

        let Some(request) = WelcomeRequest::deserialize(&request_str) else {
            let response = WelcomeResponse::MalformedRequest.serialize();
            if let Err(e) = send(raw_fd, response.as_bytes(), MsgFlags::empty()) {
                error_tx.send(Box::new(e)).unwrap();
            }
            continue;
        };

        match request {
            WelcomeRequest::OpenEnvironment(env_name) => {
                match environments.entry(env_name.clone()) {
                    Entry::Vacant(entry) => {
                        match Environment::new(&env_name, client_fd, error_tx.clone(), close_env_event.clone(), close_env_tx.clone()) {
                            Ok(res) => {
                                entry.insert(res);
                            },
                            Err(e) => {
                                error_tx.send(e).unwrap();
                                let response = WelcomeResponse::InternalError.serialize();
                                if let Err(e) = send(raw_fd, response.as_bytes(), MsgFlags::empty()) {
                                    error_tx.send(Box::new(e)).unwrap();
                                }
                                return Some(welcome_socket);
                            }
                        }
                    },
                    Entry::Occupied(mut entry) => {
                        entry.get_mut().insert_socket(client_fd);
                    },
                }
                let response = WelcomeResponse::OpenEnvironmentOk.serialize();
                if let Err(e) = send(raw_fd, response.as_bytes(), MsgFlags::empty()) {
                    error_tx.send(Box::new(e)).unwrap();
                }
            }
        }
    }

    Some(welcome_socket)
}
