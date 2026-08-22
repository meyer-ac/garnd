use crate::linux::environment::Environment;
use crate::linux::runtime_error::RuntimeError;
use crate::{send_all_errors, send_error};
use garnshared::constants::WELCOME_REQUEST_SIZE;
use garnshared::error_types::SendableError;
use garnshared::welcome_protocol::{WelcomeRequest, WelcomeResponse};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::socket::{Backlog, MsgFlags, accept, listen, recv, send};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::mpsc::Sender;
use std::sync::{Arc, mpsc};

pub fn welcome_thread_main(
    error_tx: Sender<SendableError>,
    welcome_socket: OwnedFd,
    shutdown_event: Arc<EventFd>,
) {
    // Initialization
    let mut environments = HashMap::new();
    // Environment threads trigger this event to notify the owner (this thread) about
    // a graceful shutdown
    let close_env_event = Arc::new(
        match EventFd::from_value_and_flags(0, EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK) {
            Ok(res) => res,
            Err(e) => {
                send_error!(error_tx, e);
                return;
            }
        },
    );
    // Environment threads send their name to this channel during a graceful shutdown,
    // so that the owner (this thread) can remove the environment
    let (close_env_tx, close_env_rx) = mpsc::channel::<String>();

    // todo: proper error handling

    if let Err(e) = listen(&welcome_socket.as_fd(), Backlog::MAXCONN) {
        send_error!(error_tx, e);
        return;
    }

    // Event loop
    loop {
        // Wait for events
        let poll_fds = &mut [
            PollFd::new(shutdown_event.as_fd(), PollFlags::POLLIN),
            PollFd::new(close_env_event.as_fd(), PollFlags::POLLIN),
            PollFd::new(welcome_socket.as_fd(), PollFlags::POLLIN),
        ];

        if let Err(e) = poll(poll_fds, PollTimeout::NONE) {
            send_error!(error_tx, e);
            return;
        }

        let [poll_shutdown, poll_close_env, poll_welcome] = &*poll_fds;

        // Graceful shutdown
        if poll_shutdown.any().unwrap_or_default() {
            shutdown_event
                .read()
                .map(|_| ())
                .unwrap_or_else(|e| send_error!(error_tx, e));
            return;
        }

        // Environment shutdown
        if poll_close_env.any().unwrap_or_default() {
            close_env_event
                .read()
                .map(|_| ())
                .unwrap_or_else(|e| send_error!(error_tx, e));
            let name = match close_env_rx.recv() {
                Ok(res) => res,
                Err(e) => {
                    send_error!(error_tx, e);
                    return;
                }
            };
            environments.remove(&name);
        }

        // Handle events on the welcome thread, if there are any
        if !poll_welcome.any().unwrap_or_default() {
            continue;
        }
        // Did the welcome socket break down for some reason?
        if !poll_welcome.revents().unwrap().contains(PollFlags::POLLIN) {
            send_error!(error_tx, RuntimeError::WelcomeSocketFailed);
            return; // todo: panic here?
        }

        let (client_fd, request) = match receive_and_parse_request(welcome_socket.as_fd()) {
            Ok(res) => res,
            Err(Some(e)) => {
                send_error!(error_tx, e);
                continue;
            }
            Err(None) => continue,
        };

        // Handle the requests accordingly
        match request {
            WelcomeRequest::OpenEnvironment(env_name) => handle_open_environment(
                env_name,
                &mut environments,
                &error_tx,
                client_fd,
                &close_env_event,
                &close_env_tx,
            )
            .unwrap_or_else(|es| send_all_errors!(error_tx, es)),
        }
    }
}

fn receive_and_parse_request(
    welcome_socket: BorrowedFd,
) -> Result<(OwnedFd, WelcomeRequest), Option<Errno>> {
    let raw_fd = accept(welcome_socket.as_raw_fd()).map_err(Some)?;
    // SAFETY: res is open and suitable for taking ownership; the raw fd is immediately discarded
    let client_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    let mut buffer: [u8; WELCOME_REQUEST_SIZE] = [0; WELCOME_REQUEST_SIZE];
    recv(raw_fd, &mut buffer, MsgFlags::empty())?;

    let Ok(request_str) = String::from_utf8(buffer.to_vec()) else {
        let response = WelcomeResponse::MalformedRequest.serialize();
        send(raw_fd, response.as_bytes(), MsgFlags::empty()).map_err(Some)?;
        return Err(None);
    };

    let Some(request) = WelcomeRequest::deserialize(&request_str) else {
        let response = WelcomeResponse::MalformedRequest.serialize();
        send(raw_fd, response.as_bytes(), MsgFlags::empty()).map_err(Some)?;
        return Err(None);
    };

    Ok((client_fd, request))
}

fn handle_open_environment(
    env_name: String,
    environments: &mut HashMap<String, Environment>,
    error_tx: &Sender<SendableError>,
    client_fd: OwnedFd,
    close_env_event: &Arc<EventFd>,
    close_env_tx: &Sender<String>,
) -> Result<(), Vec<SendableError>> {
    let raw_fd = client_fd.as_raw_fd();
    let infallible_open_action: Box<dyn FnOnce()>;
    match environments.entry(env_name.clone()) {
        Entry::Vacant(entry) => {
            match Environment::new(
                &env_name,
                client_fd,
                error_tx.clone(),
                close_env_event.clone(),
                close_env_tx.clone(),
            ) {
                Ok(res) => {
                    infallible_open_action = Box::new(move || {
                        entry.insert(res);
                    });
                }
                Err(e) => {
                    let mut errors = vec![e];
                    let response = WelcomeResponse::InternalError.serialize();
                    if let Err(e) = send(raw_fd, response.as_bytes(), MsgFlags::empty()) {
                        errors.push(Box::new(e));
                    }
                    return Err(errors);
                }
            }
        }
        Entry::Occupied(mut entry) => {
            infallible_open_action = Box::new(move || {
                entry.get_mut().insert_socket(client_fd);
            });
        }
    }
    let response = WelcomeResponse::OpenEnvironmentOk.serialize();
    if let Err(e) = send(raw_fd, response.as_bytes(), MsgFlags::empty()) {
        return Err(vec![Box::new(e)]);
    }
    infallible_open_action();
    Ok(())
}
