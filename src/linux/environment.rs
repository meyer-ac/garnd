use std::collections::HashSet;
use std::error::Error;
use std::io::IoSlice;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::Sender;
use garnshared::constants::ENVIRONMENT_REQUEST_SIZE;
use garnshared::environment_protocol::{EnvironmentRequest, EnvironmentResponse};
use garnshared::linux::pthread_mutex::PthreadMutex;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::eventfd::EventFd;
use nix::sys::socket::{recv, MsgFlags, send, ControlMessage, sendmsg};
use super::shm_allocator::ShmAllocator;

macro_rules! just_exit {
    ($name:expr, $error_tx:expr, $close_env_event:expr, $close_env_tx:expr) => {
        if let Err(e2) = $close_env_tx.send($name.to_owned()) {
            let _ = $error_tx.send(Box::new(e2));
        }
        if let Err(e2) = $close_env_event.write(1) {
            let _ = $error_tx.send(Box::new(e2));
        }
    };
}

macro_rules! report_error_and_close {
    ($e:expr, $name:expr, $error_tx:expr, $close_env_event:expr, $close_env_tx:expr) => {
        let _ = $error_tx.send(Box::new($e));
        just_exit!($name, $error_tx, $close_env_event, $close_env_tx);
    };
}

pub struct Environment {
    name: String,
    sockets: Arc<Mutex<HashSet<RawFd>>>,
    error_tx: Sender<Box<dyn Error + Send>>,
    close_env_event: Arc<EventFd>,
    close_env_tx: Sender<String>,
    shm: Arc<Mutex<ShmAllocator>>
}

impl Environment {
    pub fn new(name: &str, socket: OwnedFd, error_tx: Sender<Box<dyn Error + Send>>, close_env_event: Arc<EventFd>, close_env_tx: Sender<String>) -> Result<Self, Box<dyn Error + Send>> {
        let mut sockets = HashSet::new();
        sockets.insert(socket.into_raw_fd());
        Ok(Self {
            name: name.to_owned(),
            sockets: Arc::new(Mutex::new(sockets)),
            error_tx,
            close_env_event,
            close_env_tx,
            shm: Arc::new(Mutex::new(ShmAllocator::new()?)),
        })
    }

    pub fn insert_socket(&mut self, socket: OwnedFd) {
        // todo: unwrap?
        self.sockets.lock().unwrap().insert(socket.into_raw_fd());
    }

    fn thread_main(name: &str, sockets: Arc<Mutex<HashSet<RawFd>>>, error_tx: Sender<Box<dyn Error + Send>>, close_env_event: Arc<EventFd>, close_env_tx: Sender<String>, shm: Arc<Mutex<ShmAllocator>>, add_event: Arc<EventFd>) {
        // todo: unwrap?
        let epoll = match Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC) {
            Ok(res) => res,
            Err(e) => {
                report_error_and_close!(e, name, error_tx, close_env_event, close_env_tx);
                return;
            },
        };

        if let Err(e) = epoll.add(add_event.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, add_event.as_raw_fd() as u64)) {
            report_error_and_close!(e, name, error_tx, close_env_event, close_env_tx);
            return;
        }
        for fd in sockets.lock().unwrap().iter() {
            // SAFETY: The environment owns this fd and does not ever hand it out.
            // The fd is only closed once the environment drops. Before that, this thread joins the
            // dropping thread.
            if let Err(e) = epoll.add(unsafe {BorrowedFd::borrow_raw(*fd)}, EpollEvent::new(EpollFlags::EPOLLIN | EpollFlags::from_bits_truncate(nix::libc::EPOLLRDHUP), *fd as u64)) {
                report_error_and_close!(e, name, error_tx, close_env_event, close_env_tx);
                return;
            }
        }

        loop {
            let mut events = Vec::with_capacity(sockets.lock().unwrap().len() + 1);
            if let Err(e) = epoll.wait(&mut events, EpollTimeout::NONE) {
                report_error_and_close!(e, name, error_tx, close_env_event, close_env_tx);
                return;
            }
            for event in events.into_iter() {
                if event.data() == add_event.as_raw_fd() as u64 {
                    just_exit!(name, error_tx, close_env_event, close_env_tx);
                    return;
                }
                let raw_fd = event.data() as RawFd;
                if event.events() & EpollFlags::from_bits_truncate(nix::libc::EPOLLRDHUP) != EpollFlags::empty() {
                    let mut locked_sockets = sockets.lock().unwrap();
                    locked_sockets.remove(&raw_fd);
                    // SAFETY: see above + fd won't be dropped in destructor after removal from HashMap
                    drop(unsafe {OwnedFd::from_raw_fd(raw_fd)});
                    continue;
                }

                let mut buffer: [u8; ENVIRONMENT_REQUEST_SIZE] = [0; ENVIRONMENT_REQUEST_SIZE];
                if let Err(e) = recv(raw_fd, &mut buffer, MsgFlags::empty()) {
                    error_tx.send(Box::new(e)).unwrap();
                    continue;
                }

                let Ok(request_str) = String::from_utf8(buffer.to_vec()) else {
                    let response = EnvironmentResponse::MalformedRequest.serialize();
                    if let Err(e) = send(raw_fd, response.as_bytes(), MsgFlags::empty()) {
                        error_tx.send(Box::new(e)).unwrap();
                    }
                    continue;
                };

                let Some(request) = EnvironmentRequest::deserialize(&request_str) else {
                    let response = EnvironmentResponse::MalformedRequest.serialize();
                    if let Err(e) = send(raw_fd, response.as_bytes(), MsgFlags::empty()).map_err(Box::new) {
                        error_tx.send(Box::new(e)).unwrap();
                    }
                    continue;
                };

                match request {
                    EnvironmentRequest::OpenMutex(name) => {
                        let mut mutex = MaybeUninit::uninit();
                        // SAFETY: mutex was just reserved, MaybeUninit guarantees size and align.
                        if let Err(e) = unsafe {PthreadMutex::init(&raw mut mutex)} {
                            let response = EnvironmentResponse::InternalError.serialize();
                            if let Err(e2) = send(raw_fd, response.as_bytes(), MsgFlags::empty()) {
                                error_tx.send(Box::new(e2)).unwrap();
                            }
                            error_tx.send(Box::new(e)).unwrap();
                            continue;
                        }
                        let shm_location = match shm.lock().unwrap().move_into_shm(&name, unsafe { mutex.assume_init() }) {
                            Ok(res) => res,
                            Err(e) => {
                                let response = EnvironmentResponse::InternalError.serialize();
                                if let Err(e2) = send(raw_fd, response.as_bytes(), MsgFlags::empty()) {
                                    error_tx.send(Box::new(e2)).unwrap();
                                }
                                error_tx.send(e).unwrap();
                                continue;
                            }
                        };
                        let response = EnvironmentResponse::OpenMutexOk(shm_location.page, shm_location.offset).serialize();
                        let iov = [IoSlice::new(response.as_bytes())];
                        let fds = [shm_location.fd];
                        let cmsg = ControlMessage::ScmRights(&fds);
                        if let Err(e) = sendmsg::<()>(raw_fd, &iov, &[cmsg], MsgFlags::empty(), None) {
                            error_tx.send(Box::new(e)).unwrap();
                            continue;
                        }
                    }
                }
            }
        }
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for fd in self.sockets.lock().unwrap().drain() {
            // SAFETY: fd was never handed out, therefore never closed and the ownership was passed
            // to self in the constructor
            drop(unsafe {OwnedFd::from_raw_fd(fd)});
        }
    }
}