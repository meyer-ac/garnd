use super::runtime_error::RuntimeError;
use crate::error_in_brittle_scenario;
use crate::join_guard::JoinGuard;
use crate::linux::welcome_thread;
use crate::{constants, send_error};
use cfg_if::cfg_if;
use errno::{Errno, errno, set_errno};
use garnshared::error_types::{DetailedError, ResultMetadata};
use nix::errno::Errno as NixErrno;
use nix::libc;
use nix::libc::_exit;
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::prctl::get_no_new_privs;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::sys::socket::sockopt::PassCred;
use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, bind, setsockopt, socket};
use nix::unistd::{getgroups, getpid, getresgid, getresuid, setfsgid, setfsuid, Gid, Group, Uid, User};
use std::ffi::c_int;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering, compiler_fence};
use std::sync::mpsc;
use std::sync::mpsc::Sender;
use std::thread;

/// Only used for the termination and reload signal handlers, NOWHERE ELSE!
static SHUTDOWN_EVENT_FOR_SIGNAL: AtomicI32 = AtomicI32::new(-1);
static RELOAD_EVENT_FOR_SIGNAL: AtomicI32 = AtomicI32::new(-1);

pub struct Runtime<S: State> {
    error_tx: Sender<DetailedError>,
    working_dir_path: PathBuf,
    state_data: S,
}

impl Runtime<Uninit> {
    pub fn error_return_code() -> i32 {
        1
    }
    
    pub fn new(working_dir_name: Option<&str>) -> (Self, mpsc::Sender<DetailedError>, mpsc::Receiver<DetailedError>) {
        let (tx, rx) = mpsc::channel::<DetailedError>();
        let working_dir_path =
            Path::new(working_dir_name.unwrap_or(garnshared::constants::WORKING_DIR)).to_path_buf();

        (
            Self {
                error_tx: tx.clone(),
                working_dir_path,
                state_data: Uninit {},
            },
            tx,
            rx,
        )
    }

    #[allow(clippy::unused_self)]
    pub fn notify_system_starting(&self) -> Result<(), DetailedError> {
        systemd::daemon::notify(false, [("STATUS", "Starting service..."), ("MAINPID", &getpid().to_string())].iter())
            .map(|_| ())
            .add_metadata()
    }

    pub fn init(self) -> Result<Runtime<Ready>, DetailedError> {
        self.check_privileges()?;

        let welcome_socket = Self::setup_socket()?;
        let (shutdown_event, reload_event) = Self::setup_events()?;

        Ok(Runtime {
            error_tx: self.error_tx,
            working_dir_path: self.working_dir_path,
            state_data: Ready {
                welcome_socket,
                shutdown_event,
                reload_event,
            },
        })
    }

    #[allow(clippy::similar_names)] // uid and gid being similar is fine
    fn check_privileges(&self) -> Result<(), DetailedError> {
        cfg_if! {
            if #[cfg(debug_assertions)] {
                send_error!(self.error_tx, DetailedError::add_metadata(RuntimeError::PrivilegeChecksDisabled));
                return Ok(());
            }
        }
        #[allow(unreachable_code)] // Only unreachable in debug mode, which is intended
        let garn_user = User::from_name(constants::USER_NAME)
            .add_metadata()?
            .ok_or(Box::new(RuntimeError::UserNonexistent))
            .add_metadata()?;
        let res_uid = getresuid().add_metadata()?;
        if res_uid.real != garn_user.uid
            || res_uid.effective != garn_user.uid
            || res_uid.saved != garn_user.uid
        {
            return Err(RuntimeError::RunAsWrongUser).add_metadata();
        }
        if setfsuid(Uid::from_raw(u32::MAX)) != garn_user.uid {
            return Err(RuntimeError::RunAsWrongUser).add_metadata();
        }

        let garn_group = Group::from_name(constants::GROUP_NAME)
            .add_metadata()?
            .ok_or(Box::new(RuntimeError::GroupNonexistent))
            .add_metadata()?;
        let res_gid = getresgid().add_metadata()?;
        if res_gid.real != garn_group.gid
            || res_gid.effective != garn_group.gid
            || res_gid.saved != garn_group.gid
        {
            return Err(RuntimeError::RunAsWrongGroup).add_metadata();
        }
        if setfsgid(Gid::from_raw(u32::MAX)) != garn_group.gid {
            return Err(RuntimeError::RunAsWrongGroup).add_metadata();
        }
        let groups = getgroups().add_metadata()?;
        if groups.contains(&Gid::from_raw(0)) {
            return Err(RuntimeError::RunWithRootGroup).add_metadata();
        }

        for cap in &caps::all() {
            for cap_set in &[caps::CapSet::Permitted, caps::CapSet::Bounding] {
                let has_cap = caps::has_cap(None, *cap_set, *cap).add_metadata()?;
                if has_cap {
                    return Err(RuntimeError::RunWithCapabilities).add_metadata();
                }
            }
        }

        let no_new_privs = get_no_new_privs().add_metadata()?;
        if !no_new_privs {
            return Err(RuntimeError::MayObtainNewPrivileges).add_metadata();
        }

        set_errno(Errno(0));
        // SAFETY: We pass a valid value to option and zeroes everywhere else, hence the call is safe.
        let secure_bits = unsafe { libc::prctl(libc::PR_GET_SECUREBITS, 0, 0, 0, 0) };
        if secure_bits == -1 {
            return Err(std::io::Error::from_raw_os_error(errno().0)).add_metadata();
        }
        if secure_bits & libc::SECBIT_NOROOT == 0
            || secure_bits & libc::SECBIT_NOROOT_LOCKED == 0
            || secure_bits & libc::SECBIT_KEEP_CAPS > 0
            || secure_bits & libc::SECBIT_KEEP_CAPS_LOCKED == 0
            || secure_bits & libc::SECBIT_NO_SETUID_FIXUP == 0
            || secure_bits & libc::SECBIT_NO_SETUID_FIXUP_LOCKED == 0
            || secure_bits & libc::SECBIT_NO_CAP_AMBIENT_RAISE == 0
            || secure_bits & libc::SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED == 0
        {
            return Err(RuntimeError::SecureBitsNotSet).add_metadata();
        }

        Ok(())
    }

    fn setup_socket() -> Result<OwnedFd, DetailedError> {
        let welcome_socket = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .add_metadata()?;

        setsockopt(&welcome_socket.as_fd(), PassCred, &true).add_metadata()?;

        let welcome_sock_name = [
            garnshared::constants::ABSTRACT_SOCK_NAME_PREFIX,
            garnshared::constants::WELCOME_SOCK_ABSTRACT_NAME,
        ]
        .into_iter()
        .collect::<String>();
        let addr = UnixAddr::new_abstract(welcome_sock_name.as_bytes()).add_metadata()?;

        if let Err(e) = bind(welcome_socket.as_raw_fd(), &addr) {
            return match e {
                NixErrno::EADDRINUSE => Err(RuntimeError::ServiceAlreadyRunning).add_metadata(),
                e => Err(e).add_metadata(),
            };
        }

        Ok(welcome_socket)
    }

    fn setup_events() -> Result<(&'static EventFd, &'static EventFd), DetailedError> {
        // We recycle already existing `eventfd`s in order to preserve reload and shutdown requests
        // across reloads. The cost of creating new `eventfd`s for the atomic exchanges on every
        // reload is acceptable (definitely not a hot path).
        let mut shutdown_event =
            EventFd::from_value_and_flags(0, EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
                .add_metadata()?;

        let mut reload_event =
            EventFd::from_value_and_flags(0, EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
                .add_metadata()?;

        if let Err(raw_fd) = SHUTDOWN_EVENT_FOR_SIGNAL.compare_exchange(
            -1i32,
            shutdown_event.as_raw_fd(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            // SAFETY: from_raw_fd: raw_fd is a valid fd and will be subsequently leaked => no ownership is taken permanently
            // from_owned_fd: raw_fd is an EventFd
            shutdown_event = unsafe { EventFd::from_owned_fd(OwnedFd::from_raw_fd(raw_fd)) }
        }

        if let Err(raw_fd) = RELOAD_EVENT_FOR_SIGNAL.compare_exchange(
            -1i32,
            reload_event.as_raw_fd(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            // SAFETY: from_raw_fd: raw_fd is a valid fd and will be subsequently leaked => no ownership is taken permanently
            // from_owned_fd: raw_fd is an EventFd
            reload_event = unsafe { EventFd::from_owned_fd(OwnedFd::from_raw_fd(raw_fd)) }
        }

        let shutdown_event = Box::leak(Box::new(shutdown_event));
        let reload_event = Box::leak(Box::new(reload_event));

        compiler_fence(Ordering::Release);

        // Setup signal handlers for graceful shutdown and reload
        // Safety: The signal handlers are async safe.
        unsafe {
            sigaction(
                Signal::SIGTERM,
                &SigAction::new(
                    SigHandler::Handler(Self::termination_signal_handler),
                    SaFlags::SA_RESTART,
                    Signal::SIGTERM | Signal::SIGINT,
                ),
            )
        }
        .add_metadata()?;
        unsafe {
            sigaction(
                Signal::SIGINT,
                &SigAction::new(
                    SigHandler::Handler(Self::termination_signal_handler),
                    SaFlags::SA_RESTART,
                    Signal::SIGTERM | Signal::SIGINT,
                ),
            )
        }
        .add_metadata()?;
        unsafe {
            sigaction(
                Signal::SIGHUP,
                &SigAction::new(
                    SigHandler::Handler(Self::reload_signal_handler),
                    SaFlags::SA_RESTART,
                    SigSet::from(Signal::SIGHUP),
                ),
            )
        }
        .add_metadata()?;

        Ok((shutdown_event, reload_event))
    }

    /// This function is async safe.
    extern "C" fn termination_signal_handler(_signal: c_int) {
        Self::termination_or_reload_handler(false);
    }

    /// This function is async safe.
    extern "C" fn reload_signal_handler(_signal: c_int) {
        Self::termination_or_reload_handler(true);
    }

    /// This function is async safe
    extern "C" fn termination_or_reload_handler(is_reload: bool) {
        compiler_fence(Ordering::Acquire);
        let fd;
        if is_reload {
            // Safety: static read operation backed by static's safety invariant;
            fd = RELOAD_EVENT_FOR_SIGNAL.load(Ordering::Relaxed);
            if fd == -1 {
                error_in_brittle_scenario(
                    "Reload requested in an early or severely invalid state of the program, continuing.",
                );
                return;
            }
        } else {
            // Safety: static read operation backed by static's safety invariant;
            fd = SHUTDOWN_EVENT_FOR_SIGNAL.load(Ordering::Relaxed);
            if fd == -1 {
                error_in_brittle_scenario(
                    "Termination requested in an early or severely invalid state of the program, exiting.",
                );
                // Safety: Potentially ill-formed program states are irrelevant here, because we exit immediately anyway
                unsafe {
                    _exit(-1);
                }
            }
        }
        let buf = 1u64;
        // Safety: a write operation to an invalid fd cannot cause UB;
        // the value written to it is exactly 8 bytes;
        // `write` is async safe.
        unsafe {
            let last_errno = *libc::__errno_location();
            let _ = libc::write(
                fd,
                (&raw const buf).cast::<libc::c_void>(),
                size_of_val(&buf),
            );
            *libc::__errno_location() = last_errno;
        }
    }
}

impl Runtime<Ready> {
    pub fn listen(self) -> Result<Runtime<Listening>, DetailedError> {
        // Ownership of the socket is moved into the thread and handed back once the threads join.
        let error_tx = self.error_tx.clone();
        let welcome_socket = self.state_data.welcome_socket;
        let shutdown_event = self.state_data.shutdown_event;
        let reload_event = self.state_data.reload_event;
        //let welcome_thread = thread::spawn(move || {
        //    welcome_thread::welcome_thread_main(error_tx, welcome_socket, shutdown_event)
        //});
        let welcome_thread = JoinGuard::from(
            thread::Builder::new()
                .spawn(move || {
                    welcome_thread::welcome_thread_main(
                        &error_tx,
                        welcome_socket,
                        shutdown_event,
                        reload_event,
                    );
                })
                .add_metadata()?,
        );

        Ok(Runtime {
            error_tx: self.error_tx,
            working_dir_path: self.working_dir_path,
            state_data: Listening {
                _welcome_thread: welcome_thread,
            },
        })
    }
}

impl Runtime<Listening> {
    #[allow(clippy::unused_self)]
    pub fn notify_system_listening(&self) -> Result<(), DetailedError> {
        systemd::daemon::notify(false, [("READY", "1"), ("STATUS", "Listening...")].iter())
            .map(|_| ())
            .add_metadata()
    }

    #[allow(clippy::unused_self)]
    pub fn notify_system_reloading(&self) -> Result<(), DetailedError> {
        systemd::daemon::notify(false, [("RELOADING", "1"), ("STATUS", "Reloading service...")].iter())
            .map(|_| ())
            .add_metadata()
    }

    #[allow(clippy::unused_self)]
    pub fn notify_system_stopping(&self) -> Result<(), DetailedError> {
        systemd::daemon::notify(false, [("STOPPING", "1"), ("STATUS", "Stopping service...")].iter())
            .map(|_| ())
            .add_metadata()
    }
}

pub trait State {}
pub struct Uninit {}
pub struct Ready {
    welcome_socket: OwnedFd,
    // References are stored because the fds are leaked to survive reloads and only get destroyed on exit
    shutdown_event: &'static EventFd,
    reload_event: &'static EventFd,
}
pub struct Listening {
    _welcome_thread: JoinGuard,
}

impl State for Uninit {}
impl State for Ready {}
impl State for Listening {}
