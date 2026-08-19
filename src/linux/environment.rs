use std::collections::HashSet;
use std::error::Error;
use std::hash::Hash;
use std::io::IoSlice;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::{mpsc, Arc, Mutex};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::thread::JoinHandle;
use garnshared::constants::ENVIRONMENT_REQUEST_SIZE;
use garnshared::environment_protocol::{EnvironmentRequest, EnvironmentResponse};
use garnshared::linux::pthread_mutex::PthreadMutex;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::socket::{recv, MsgFlags, send, ControlMessage, sendmsg};
use super::shm_allocator::ShmAllocator;

macro_rules! report_boxed_error_and_close {
    ($e:expr, $name:expr, $error_tx:expr, $close_env_event:expr, $close_env_tx:expr) => {
        let _ = $error_tx.send($e);
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
        report_boxed_error_and_close!(Box::new($e), $name, $error_tx, $close_env_event, $close_env_tx);
    };
}

// todo: is this necessary or does it suffice to store this in the thread main?
pub struct Environment {
    add_listener_event: Arc<EventFd>,
    add_listener_tx: Sender<OwnedFd>,
    drop_event: Arc<EventFd>,
    thread: ManuallyDrop<JoinHandle<()>>,
}

impl Environment {
    pub fn new(name: &str, socket: OwnedFd, error_tx: Sender<Box<dyn Error + Send>>, close_env_event: Arc<EventFd>, close_env_tx: Sender<String>) -> Result<Self, Box<dyn Error + Send>> {
        let name = name.to_owned();
        let add_listener_event = Arc::new(match EventFd::from_value_and_flags(
            0,
            EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK,
        ) {
            Ok(res) => res,
            Err(e) => return Err(Box::new(e)),
        });
        let (add_listener_tx, add_listener_rx) = mpsc::channel::<OwnedFd>();
        let drop_event = Arc::new(match EventFd::from_value_and_flags(
            0,
            EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK,
        ) {
            Ok(res) => res,
            Err(e) => return Err(Box::new(e)),
        });
        let thread_add_listener_event = add_listener_event.clone();
        let thread_drop_event = drop_event.clone();
        let thread = ManuallyDrop::new(thread::spawn(move || {
           Self::thread_main(&name, error_tx, close_env_event, close_env_tx, thread_add_listener_event, add_listener_rx, thread_drop_event);
        }));
        // todo: unwrap?
        add_listener_tx.send(socket).unwrap();
        add_listener_event.write(1).unwrap();
        Ok(Self {
            add_listener_event,
            add_listener_tx,
            drop_event,
            thread,
        })
    }

    pub fn insert_socket(&mut self, socket: OwnedFd) {
        // todo: unwrap?
        self.add_listener_tx.send(socket).unwrap();
        self.add_listener_event.write(1).unwrap();
    }

    fn thread_main(name: &str, error_tx: Sender<Box<dyn Error + Send>>, close_env_event: Arc<EventFd>, close_env_tx: Sender<String>, add_listener_event: Arc<EventFd>, add_listener_rx: Receiver<OwnedFd>, drop_event: Arc<EventFd>) {
        // todo: unwrap?
        let mut sockets = HashSet::<RawFd>::new();
        let mut shm = match ShmAllocator::new() {
            Ok(res) => res,
            Err(e) => {
                report_boxed_error_and_close!(e, name, error_tx, close_env_event, close_env_tx);
                return;
            },
        };

        let epoll = match Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC) {
            Ok(res) => res,
            Err(e) => {
                report_error_and_close!(e, name, error_tx, close_env_event, close_env_tx);
                return;
            },
        };

        if let Err(e) = epoll.add(add_listener_event.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, add_listener_event.as_raw_fd() as u64)) {
            report_error_and_close!(e, name, error_tx, close_env_event, close_env_tx);
            return;
        }
        if let Err(e) = epoll.add(drop_event.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, drop_event.as_raw_fd() as u64)) {
            report_error_and_close!(e, name, error_tx, close_env_event, close_env_tx);
            return;
        }

        let mut break_loop = false;
        while !break_loop {
            let mut events = vec![EpollEvent::empty(); sockets.len() + 1];
            let num_events = match epoll.wait(&mut events, EpollTimeout::NONE) {
                Ok(res) => res,
                Err(e) => {
                    report_error_and_close!(e, name, error_tx, close_env_event, close_env_tx);
                    return;
                }
            };
            for event in events[..num_events].into_iter() {
                if event.data() == add_listener_event.as_raw_fd() as u64 {
                    // ???
                    //just_exit!(name, error_tx, close_env_event, close_env_tx);
                    //return;
                    if let Err(e) = add_listener_event.read() {
                        report_error_and_close!(e, name, error_tx, close_env_event, close_env_tx);
                        return;
                    }
                    let new_listener = add_listener_rx.recv().unwrap();
                    if let Err(e) = epoll.add(new_listener.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN | EpollFlags::from_bits_truncate(nix::libc::EPOLLRDHUP), new_listener.as_raw_fd() as u64)) {
                        report_error_and_close!(e, name, error_tx, close_env_event, close_env_tx);
                        return;
                    }
                    sockets.insert(new_listener.into_raw_fd());
                    continue;
                }
                if event.data() == drop_event.as_raw_fd() as u64 {
                    if let Err(e) = drop_event.read() {
                        let _ = error_tx.send(Box::new(e));
                    }
                    break_loop = true;
                    break;
                }
                let raw_fd = event.data() as RawFd;
                if event.events() & EpollFlags::from_bits_truncate(nix::libc::EPOLLRDHUP) != EpollFlags::empty() {
                    sockets.remove(&raw_fd);
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
                        let shm_location = match shm.find_resource::<PthreadMutex>(&name) {
                            Ok(Some(res)) => res,
                            Ok(None) => {
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
                                match shm.move_into_shm(&name, unsafe { mutex.assume_init() }) {
                                    Ok(res) => res,
                                    Err(e) => {
                                        let response = EnvironmentResponse::InternalError.serialize();
                                        if let Err(e2) = send(raw_fd, response.as_bytes(), MsgFlags::empty()) {
                                            error_tx.send(Box::new(e2)).unwrap();
                                        }
                                        error_tx.send(e).unwrap();
                                        continue;
                                    }
                                }
                            },
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

        for fd in sockets.drain() {
            // SAFETY: fd was never handed out, therefore never closed and the ownership was passed
            // to self in the constructor
            drop(unsafe {OwnedFd::from_raw_fd(fd)});
        }
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        self.drop_event.write(1).unwrap();
        // SAFETY: The field is not accessed after this called, because the lifetime of the
        // whole Environment object ends here.
        unsafe {ManuallyDrop::take(&mut self.thread)}.join().unwrap();
    }
}