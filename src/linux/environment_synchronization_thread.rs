use crate::linux::shm_allocator::ShmAllocator;
use crate::linux::util::unwrap_or_report_failure;
use crate::{send_all_errors, send_error};
use garnshared::environment_protocol::{EnvironmentRequest, EnvironmentResponse, ENVIRONMENT_REQUEST_PROTOCOL};
use garnshared::error_types::SendableError;
use garnshared::linux::pthread_mutex::PthreadMutex;
use nix::errno::Errno;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::eventfd::EventFd;
use nix::sys::socket::{ControlMessage, MsgFlags, recv, send, sendmsg};
use std::collections::HashSet;
use std::io::IoSlice;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

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

macro_rules! pass_result_to_requesting_thread {
    ($res:expr, $response_tx:expr, $error_tx:expr) => {
        let _ = $res
            .map(|_| {
                let _ = $response_tx
                    .send(Ok(()))
                    .map_err(|e| send_error!($error_tx, e));
            })
            .map_err(|e| {
                let _ = $response_tx
                    .send(Err(Box::new(e)))
                    .map_err(|e| send_error!($error_tx, e));
            });
    };
}

#[allow(clippy::too_many_arguments)]
pub fn environment_synchronization_thread_main(
    name: &str,
    error_tx: &Sender<SendableError>,
    sync_response_tx: &Sender<Result<(), SendableError>>,
    close_env_event: &Arc<EventFd>,
    close_env_tx: &Sender<String>,
    add_listener_event: &Arc<EventFd>,
    add_listener_rx: &Receiver<OwnedFd>,
    drop_event: &Arc<EventFd>,
) {
    // Initialization
    let mut sockets = SocketSet::new();
    let mut shm = match ShmAllocator::new() {
        Ok(res) => res,
        Err(e) => {
            report_boxed_error_and_close(e, name, error_tx, close_env_event, close_env_tx);
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
    for event in [add_listener_event, drop_event] {
        if let Err(e) = epoll.add(
            event.as_fd(),
            #[allow(clippy::cast_sign_loss)]
            EpollEvent::new(EpollFlags::EPOLLIN, event.as_raw_fd() as u64),
        ) {
            report_error_and_close!(e, name, &error_tx, &close_env_event, &close_env_tx);
            return;
        }
    }

    // Event loop
    let mut break_loop = false;
    while !break_loop {
        let mut events = vec![EpollEvent::empty(); sockets.len() + 2];
        let num_events = match epoll.wait(&mut events, EpollTimeout::NONE) {
            Ok(res) => res,
            Err(e) => {
                report_error_and_close!(e, name, &error_tx, &close_env_event, &close_env_tx);
                return;
            }
        };
        for event in &events[..num_events] {
            // Add a new listener
            #[allow(clippy::cast_sign_loss)]
            if event.data() == add_listener_event.as_raw_fd() as u64 {
                pass_result_to_requesting_thread!(
                    add_listener(add_listener_event, add_listener_rx, &mut sockets, &epoll),
                    sync_response_tx,
                    error_tx
                );
                continue;
            }

            // Shut down the environment
            #[allow(clippy::cast_sign_loss)]
            if event.data() == drop_event.as_raw_fd() as u64 {
                drop_event
                    .read()
                    .map_or_else(|e| send_error!(error_tx, e), |_| ());
                break_loop = true;
                break;
            }

            #[allow(clippy::cast_possible_truncation)]
            let raw_fd = event.data() as RawFd;

            // Did the client close the connection?
            // Note: It's intended that we first check this and accept a potentially lost request
            // as a consequence, as it can't be responded to anyway.
            if event.events() & EpollFlags::from_bits_truncate(nix::libc::EPOLLRDHUP)
                != EpollFlags::empty()
            {
                sockets.remove(raw_fd);
                if sockets.is_empty() {
                    announce_env_close(name, close_env_event, close_env_tx)
                        .unwrap_or_else(|es| send_all_errors!(error_tx, es));
                    break_loop = true;
                    break;
                }
                continue;
            }

            // Handle client request
            let request = match receive_and_parse_request(raw_fd) {
                Ok(res) => res,
                Err(e) => {
                    send_all_errors!(error_tx, e);
                    continue;
                }
            };

            if let Err(es) = match request {
                EnvironmentRequest::OpenMutex(name) => handle_open_mutex(&name, &mut shm, raw_fd),
            } {
                send_all_errors!(error_tx, es);
            }
        }
    }
}

fn add_listener(
    add_listener_event: &Arc<EventFd>,
    add_listener_rx: &Receiver<OwnedFd>,
    sockets: &mut SocketSet,
    epoll: &Epoll,
) -> Result<(), Errno> {
    add_listener_event.read()?;
    // We don't use try_iter() here because we must distinguish between an empty and a faulty channel
    // and iter() because we must prevent subtle races arising from the not mutually synced close_env_event and close_env_rx
    loop {
        let recv_res = add_listener_rx.try_recv();
        match recv_res {
            Ok(new_listener) => {
                epoll.add(
                    new_listener.as_fd(),
                    #[allow(clippy::cast_sign_loss)]
                    EpollEvent::new(
                        EpollFlags::EPOLLIN | EpollFlags::from_bits_truncate(nix::libc::EPOLLRDHUP),
                        new_listener.as_raw_fd() as u64,
                    ),
                )?;
                sockets.insert(new_listener);
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                // The only way this can happen is if the owning thread delegating new listeners to
                // this thread has died. This is terrible and means that the program is in an already
                // unrecoverable state => escalate and panic!
                Err::<OwnedFd, TryRecvError>(TryRecvError::Disconnected).unwrap();
            }
        }
    }
    Ok(())
}

fn receive_and_parse_request(raw_fd: RawFd) -> Result<EnvironmentRequest, Vec<SendableError>> {
    let mut buffer = vec![0u8; ENVIRONMENT_REQUEST_PROTOCOL.max_size()].into_boxed_slice();
    recv(raw_fd, &mut buffer, MsgFlags::empty()).map_err(|e| vec![Box::from(e)])?;

    let request_str = match str::from_utf8(&buffer) {
        Ok(res) => res,
        Err(e) => {
            let mut errors = vec![Box::from(e)];
            let Ok(response) = EnvironmentResponse::MalformedRequest
                .serialize()
                .map_err(|e| errors.push(Box::from(e)))
            else {
                return Err(errors);
            };
            send(raw_fd, response.as_bytes(), MsgFlags::empty())
                .map_or_else(|e| errors.push(Box::from(e)), |_| ());
            return Err(errors);
        }
    };

    let request = match EnvironmentRequest::deserialize(request_str) {
        Ok(res) => res,
        Err(e) => {
            let mut errors = vec![Box::from(e)];
            let Ok(response) = EnvironmentResponse::MalformedRequest
                .serialize()
                .map_err(|e| errors.push(Box::from(e)))
            else {
                return Err(errors);
            };
            send(raw_fd, response.as_bytes(), MsgFlags::empty())
                .map_or_else(|e| errors.push(Box::from(e)), |_| ());
            return Err(errors);
        }
    };

    Ok(request)
}

fn handle_open_mutex(
    name: &str,
    shm: &mut ShmAllocator,
    raw_fd: RawFd,
) -> Result<(), Vec<SendableError>> {
    let shm_location = match unwrap_or_report_failure!(
        shm.find_resource::<PthreadMutex>(name),
        raw_fd,
        EnvironmentResponse
    ) {
        // Mutex already exists
        Some(res) => res,
        // Mutex doesn't exist yet
        None => {
            unwrap_or_report_failure!(
                shm.construct_in_shm(name, PthreadMutex::init),
                raw_fd,
                EnvironmentResponse
            )
        }
    };

    // Pass shared memory page to client
    let response = unwrap_or_report_failure!(
        EnvironmentResponse::OpenMutexOk(shm_location.page, shm_location.offset)
            .serialize()
            .map_err(Box::from),
        raw_fd,
        EnvironmentResponse
    );
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
        .map_or_else(|e| errors.push(Box::new(e)), |_| ());
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

    fn remove(&mut self, socket: RawFd) -> bool {
        if self.sockets.remove(&socket) {
            // SAFETY: see above + fd won't be dropped in destructor after removal from HashMap
            drop(unsafe { OwnedFd::from_raw_fd(socket) });
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
