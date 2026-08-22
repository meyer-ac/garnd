use crate::linux::shm_allocator::ShmAllocator;
use crate::{send_all_errors, send_error};
use garnshared::constants::ENVIRONMENT_REQUEST_SIZE;
use garnshared::environment_protocol::{EnvironmentRequest, EnvironmentResponse};
use garnshared::linux::pthread_mutex::PthreadMutex;
use nix::errno::Errno;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::eventfd::EventFd;
use nix::sys::socket::{ControlMessage, MsgFlags, recv, send, sendmsg};
use std::collections::HashSet;
use std::error::Error;
use std::io::IoSlice;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use garnshared::error_types::SendableError;

macro_rules! report_error_and_close {
    ($e:expr, $name:expr, $error_tx:expr, $close_env_event:expr, $close_env_tx:expr) => {
        report_boxed_error_and_close(
            Box::new($e),
            $name,
            $error_tx,
            $close_env_event,
            $close_env_tx,
        );
    };
}

pub fn environment_thread_main(
    name: &str,
    error_tx: Sender<SendableError>,
    close_env_event: Arc<EventFd>,
    close_env_tx: Sender<String>,
    add_listener_event: Arc<EventFd>,
    add_listener_rx: Receiver<OwnedFd>,
    drop_event: Arc<EventFd>,
) {
    // Initialization
    let mut sockets = SocketSet::new();
    let mut shm = match ShmAllocator::new() {
        Ok(res) => res,
        Err(e) => {
            report_boxed_error_and_close(e, name, &error_tx, &close_env_event, &close_env_tx);
            return;
        }
    };

    let epoll = match Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC) {
        Ok(res) => res,
        Err(e) => {
            report_error_and_close!(e, name, &error_tx, &close_env_event, &close_env_tx);
            return;
        }
    };

    // Set up listeners for "special events", namely
    // - add_listener_event: Attach a new process to the environment
    // - drop_event: Gracefully shut down the environment
    for event in [&add_listener_event, &drop_event].iter() {
        if let Err(e) = epoll.add(
            event.as_fd(),
            EpollEvent::new(EpollFlags::EPOLLIN, event.as_raw_fd() as u64),
        ) {
            report_error_and_close!(e, name, &error_tx, &close_env_event, &close_env_tx);
            return;
        }
    }

    // Event loop
    let mut break_loop = false;
    while !break_loop {
        let mut events = vec![EpollEvent::empty(); sockets.len() + 1];
        let num_events = match epoll.wait(&mut events, EpollTimeout::NONE) {
            Ok(res) => res,
            Err(e) => {
                report_error_and_close!(e, name, &error_tx, &close_env_event, &close_env_tx);
                return;
            }
        };
        for event in events[..num_events].into_iter() {
            if event.data() == add_listener_event.as_raw_fd() as u64 {
                add_listener(
                    name,
                    &add_listener_event,
                    &add_listener_rx,
                    &mut sockets,
                    &epoll,
                )
                .unwrap_or_else(|e| send_error!(error_tx, e));
                continue;
            }
            if event.data() == drop_event.as_raw_fd() as u64 {
                drop_event
                    .read()
                    .map(|_| ())
                    .unwrap_or_else(|e| send_error!(error_tx, e));
                break_loop = true;
                break;
            }

            let raw_fd = event.data() as RawFd;
            // Did the client close the connection?
            if event.events() & EpollFlags::from_bits_truncate(nix::libc::EPOLLRDHUP)
                != EpollFlags::empty()
            {
                sockets.remove(&raw_fd);
                if sockets.is_empty() {
                    announce_env_close(name, &close_env_event, &close_env_tx)
                        .unwrap_or_else(|es| send_all_errors!(error_tx, es));
                    break_loop = true;
                    break;
                }
                continue;
            }

            // Handle client request
            let request = match receive_and_parse_request(raw_fd) {
                Ok(res) => res,
                Err(Some(e)) => {
                    send_error!(error_tx, e);
                    continue;
                }
                Err(None) => continue,
            };

            match request {
                EnvironmentRequest::OpenMutex(name) =>
                    handle_open_mutex(name, &mut shm, raw_fd)
                        .unwrap_or_else(|es| send_all_errors!(error_tx, es)),
            }
        }
    }
}

fn add_listener(
    name: &str,
    add_listener_event: &Arc<EventFd>,
    add_listener_rx: &Receiver<OwnedFd>,
    sockets: &mut SocketSet,
    epoll: &Epoll,
) -> Result<(), Errno> {
    add_listener_event.read()?;
    let new_listener = add_listener_rx.recv().unwrap();
    epoll.add(
        new_listener.as_fd(),
        EpollEvent::new(
            EpollFlags::EPOLLIN | EpollFlags::from_bits_truncate(nix::libc::EPOLLRDHUP),
            new_listener.as_raw_fd() as u64,
        ),
    )?;
    sockets.insert(new_listener);
    Ok(())
}

fn receive_and_parse_request(raw_fd: RawFd) -> Result<EnvironmentRequest, Option<Errno>> {
    let mut buffer: [u8; ENVIRONMENT_REQUEST_SIZE] = [0; ENVIRONMENT_REQUEST_SIZE];
    recv(raw_fd, &mut buffer, MsgFlags::empty()).map_err(Some)?;

    let Ok(request_str) = String::from_utf8(buffer.to_vec()) else {
        let response = EnvironmentResponse::MalformedRequest.serialize();
        send(raw_fd, response.as_bytes(), MsgFlags::empty()).map_err(Some)?;
        return Err(None);
    };

    let Some(request) = EnvironmentRequest::deserialize(&request_str) else {
        let response = EnvironmentResponse::MalformedRequest.serialize();
        send(raw_fd, response.as_bytes(), MsgFlags::empty()).map_err(Some)?;
        return Err(None);
    };

    Ok(request)
}

fn handle_open_mutex(
    name: String,
    shm: &mut ShmAllocator,
    raw_fd: RawFd,
) -> Result<(), Vec<SendableError>> {
    let shm_location = match shm.find_resource::<PthreadMutex>(&name) {
        // Mutex already exists
        Ok(Some(res)) => res,
        // Mutex doesn't exist yet
        Ok(None) => {
            let mut mutex = MaybeUninit::uninit();
            // SAFETY: mutex was just reserved, MaybeUninit guarantees size and align.
            if let Err(e) = unsafe { PthreadMutex::init(&raw mut mutex) } {
                let mut errors: Vec<SendableError> = vec![Box::new(e)];
                let response = EnvironmentResponse::InternalError.serialize();
                send(raw_fd, response.as_bytes(), MsgFlags::empty())
                    .map(|_| ())
                    .unwrap_or_else(|e2| errors.push(Box::new(e2)));
                return Err(errors);
            }
            // SAFETY: mutex was just initialized and the result checked
            match shm.move_into_shm(&name, unsafe { mutex.assume_init() }) {
                Ok(res) => res,
                Err(e) => {
                    let mut errors: Vec<SendableError> = vec![e];
                    let response = EnvironmentResponse::InternalError.serialize();
                    send(raw_fd, response.as_bytes(), MsgFlags::empty())
                        .map(|_| ())
                        .unwrap_or_else(|e2| errors.push(Box::new(e2)));
                    return Err(errors);
                }
            }
        }
        Err(e) => {
            let mut errors: Vec<SendableError> = vec![e];
            let response = EnvironmentResponse::InternalError.serialize();
            send(raw_fd, response.as_bytes(), MsgFlags::empty())
                .map(|_| ())
                .unwrap_or_else(|e2| errors.push(Box::new(e2)));
            return Err(errors);
        }
    };

    // Pass shared memory page to client
    let response =
        EnvironmentResponse::OpenMutexOk(shm_location.page, shm_location.offset).serialize();
    let iov = [IoSlice::new(response.as_bytes())];
    let fds = [shm_location.fd];
    let cmsg = ControlMessage::ScmRights(&fds);
    sendmsg::<()>(raw_fd, &iov, &[cmsg], MsgFlags::empty(), None)
        .map(|_| ())
        .map_err(|e| -> Vec<SendableError> { vec![Box::new(e)] })?;

    Ok(())
}

fn announce_env_close(
    name: &str,
    close_env_event: &Arc<EventFd>,
    close_env_tx: &Sender<String>,
) -> Result<(), Vec<SendableError>> {
    let mut errors: Vec<SendableError> = vec![];
    close_env_tx
        .send(name.to_owned())
        .unwrap_or_else(|e| errors.push(Box::new(e)));
    close_env_event
        .write(1)
        .map(|_| ())
        .unwrap_or_else(|e| errors.push(Box::new(e)));
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn report_boxed_error_and_close(
    e: SendableError,
    name: &str,
    error_tx: &Sender<SendableError>,
    close_env_event: &Arc<EventFd>,
    close_env_tx: &Sender<String>,
) {
    let mut errors: Vec<SendableError> = vec![];
    error_tx
        .send(e)
        .unwrap_or_else(|e| errors.push(Box::new(e)));
    announce_env_close(name, close_env_event, close_env_tx)
        .unwrap_or_else(|ref mut es| errors.append(es));
    send_all_errors!(error_tx, errors);
}

struct SocketSet {
    sockets: HashSet<RawFd>,
}

impl SocketSet {
    fn new() -> SocketSet {
        Self {
            sockets: HashSet::<RawFd>::new(),
        }
    }

    fn len(&self) -> usize {
        self.sockets.len()
    }

    fn insert(&mut self, socket: OwnedFd) -> bool {
        self.sockets.insert(socket.into_raw_fd())
    }

    fn remove(&mut self, socket: &RawFd) -> bool {
        if self.sockets.remove(&socket) {
            // SAFETY: see above + fd won't be dropped in destructor after removal from HashMap
            drop(unsafe { OwnedFd::from_raw_fd(*socket) });
            true
        } else {
            false
        }
    }

    fn is_empty(&self) -> bool {
        self.sockets.is_empty()
    }
}

impl Drop for SocketSet {
    fn drop(&mut self) {
        for fd in self.sockets.drain() {
            // SAFETY: fd was never handed out, therefore never closed and the ownership was passed
            // to self in the constructor
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
    }
}
