use super::runtime_error::RuntimeError;
use crate::join_guard::JoinGuard;
use crate::linux::welcome_thread;
use crate::util::error_in_brittle_scenario;
use crate::{constants, send_error};
use cfg_if::cfg_if;
use errno::{Errno, errno, set_errno};
use garnshared::error_types::{ResultMetadata, DetailedError};
use nix::errno::Errno as NixErrno;
use nix::libc;
use nix::libc::_exit;
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::prctl::get_no_new_privs;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::sys::socket::sockopt::PassCred;
use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, bind, setsockopt, socket};
use nix::sys::stat::{Mode, SFlag, lstat};
use nix::unistd::{Gid, Group, Uid, User, getgroups, getresgid, getresuid, setfsgid, setfsuid};
use std::ffi::c_int;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, mpsc};
use std::{fs, thread};

/// Only used for the termination and reload signal handlers, NOWHERE ELSE!
/// # SAFETY
/// Each only written to once before the signal handler is installed.
static mut SHUTDOWN_EVENT_FOR_SIGNAL: c_int = -1;
static mut RELOAD_EVENT_FOR_SIGNAL: c_int = -1;

pub struct Runtime<S: State> {
    error_tx: Sender<DetailedError>,
    working_dir_path: PathBuf,
    state_data: S,
}

impl Runtime<Uninit> {
    pub fn new(working_dir_name: Option<&str>) -> (Self, mpsc::Receiver<DetailedError>) {
        let (tx, rx) = mpsc::channel::<DetailedError>();
        let working_dir_path =
            Path::new(working_dir_name.unwrap_or(garnshared::constants::WORKING_DIR)).to_path_buf();

        (
            Self {
                error_tx: tx,
                working_dir_path,
                state_data: Uninit {},
            },
            rx,
        )
    }

    pub fn init(self) -> Result<Runtime<Ready>, DetailedError> {
        self.check_privileges()?;

        self.setup_working_dir()?;

        let (welcome_socket, shutdown_event, reload_event) = Self::setup_socket()?;

        Ok(Runtime {
            error_tx: self.error_tx,
            working_dir_path: self.working_dir_path,
            state_data: Ready {
                welcome_socket,
                shutdown_event: Arc::new(shutdown_event),
                reload_event: Arc::new(reload_event),
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
        let garn_user = User::from_name(constants::USER_NAME).add_metadata()?
            .ok_or(Box::new(RuntimeError::UserNonexistent)).add_metadata()?;
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

        let garn_group = Group::from_name(constants::GROUP_NAME).add_metadata()?
            .ok_or(Box::new(RuntimeError::GroupNonexistent)).add_metadata()?;
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

    fn setup_working_dir(&self) -> Result<(), DetailedError> {
        let working_dir_str = self
            .working_dir_path
            .clone()
            .into_os_string()
            .into_string()
            .map_err(|_| RuntimeError::WorkingDirPathInvalidString).add_metadata()?;
        if !fs::exists(&self.working_dir_path).add_metadata()? {
            return Err(RuntimeError::WorkingDirNonexistent {
                working_dir: working_dir_str,
            }).add_metadata();
        }
        let stats = lstat(&self.working_dir_path).add_metadata()?;
        if !SFlag::from_bits_truncate(stats.st_mode).contains(SFlag::S_IFDIR) {
            return Err(RuntimeError::WorkingDirNotADirectory {
                working_dir: working_dir_str,
            }).add_metadata();
        }

        cfg_if! {
            if #[cfg(debug_assertions)] {
                return Ok(())
            }
        }

        #[allow(unreachable_code)] // Only reachable in release mode, intended

        // Verify owner
        let garn_user = User::from_name(constants::USER_NAME).add_metadata()?
            .ok_or(Box::new(RuntimeError::UserNonexistent)).add_metadata()?;
        let garn_group = Group::from_name(constants::GROUP_NAME).add_metadata()?
            .ok_or(Box::new(RuntimeError::GroupNonexistent)).add_metadata()?;
        let owner_user = User::from_uid(Uid::from_raw(stats.st_uid)).add_metadata()?.unwrap();
        let owner_group = Group::from_gid(Gid::from_raw(stats.st_gid)).add_metadata()?.unwrap();
        if owner_user.uid != garn_user.uid {
            return Err(RuntimeError::WorkingDirOwnedByWrongUser {
                working_dir: working_dir_str,
                owner: owner_user.name,
            }).add_metadata();
        }
        if owner_group.gid != garn_group.gid {
            return Err(RuntimeError::WorkingDirOwnedByWrongGroup {
                working_dir: working_dir_str,
                owner: owner_user.name,
            }).add_metadata();
        }

        // Verify permissions
        let mode = Mode::from_bits_truncate(stats.st_mode);
        if !(mode.contains(Mode::S_IRWXU | Mode::S_IRGRP | Mode::S_IXGRP | Mode::S_IROTH | Mode::S_IXOTH) && !mode.contains(Mode::S_IWGRP) && !mode.contains(Mode::S_IWOTH)) {
            return Err(RuntimeError::WorkingDirWrongPermissions {working_dir: working_dir_str, permissions: "rwxr-xr-x"}).add_metadata();
        }
        if mode.contains(Mode::S_ISUID) {
            return Err(RuntimeError::WorkingDirSetUidBitSet {working_dir: working_dir_str}).add_metadata();
        }
        if mode.contains(Mode::S_ISGID) {
            return Err(RuntimeError::WorkingDirSetGidBitSet {working_dir: working_dir_str}).add_metadata();
        }
        if mode.contains(Mode::S_ISVTX) {
            return Err(RuntimeError::WorkingDirStickyBitSet {working_dir: working_dir_str}).add_metadata();
        }

        Ok(())
    }

    fn setup_socket() -> Result<(OwnedFd, EventFd, EventFd), DetailedError> {
        let welcome_socket = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        ).add_metadata()?;

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

        let shutdown_event = EventFd::from_value_and_flags(
            0,
            EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK,
        ).add_metadata()?;

        let reload_event = EventFd::from_value_and_flags(
            0,
            EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK,
        ).add_metadata()?;

        Ok((welcome_socket, shutdown_event, reload_event))
    }
}

impl Runtime<Ready> {
    pub fn listen(self) -> Result<Runtime<Listening>, DetailedError> {
        // Setup signal handlers for graceful shutdown and reload
        // Safety: This is the only write to the statics before the signal handlers are installed.
        unsafe {
            SHUTDOWN_EVENT_FOR_SIGNAL = self.state_data.shutdown_event.as_raw_fd();
        }unsafe {
            RELOAD_EVENT_FOR_SIGNAL = self.state_data.reload_event.as_raw_fd();
        }
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
        }.add_metadata()?;
        unsafe {
            sigaction(
                Signal::SIGINT,
                &SigAction::new(
                    SigHandler::Handler(Self::termination_signal_handler),
                    SaFlags::SA_RESTART,
                    Signal::SIGTERM | Signal::SIGINT,
                ),
            )
        }.add_metadata()?;
        unsafe {
            sigaction(
                Signal::SIGHUP,
                &SigAction::new(
                    SigHandler::Handler(Self::reload_signal_handler),
                    SaFlags::SA_RESTART,
                    SigSet::from(Signal::SIGHUP)
                ),
            )
        }.add_metadata()?;

        // Ownership of the socket is moved into the thread and handed back once the threads join.
        let error_tx = self.error_tx.clone();
        let welcome_socket = self.state_data.welcome_socket;
        let shutdown_event = Arc::clone(&self.state_data.shutdown_event);
        let reload_event = Arc::clone(&self.state_data.reload_event);
        //let welcome_thread = thread::spawn(move || {
        //    welcome_thread::welcome_thread_main(error_tx, welcome_socket, shutdown_event)
        //});
        let welcome_thread = JoinGuard::from(thread::Builder::new().spawn(move || {
            welcome_thread::welcome_thread_main(&error_tx, welcome_socket, &shutdown_event, &reload_event);
        }).add_metadata()?);

        Ok(Runtime {
            error_tx: self.error_tx,
            working_dir_path: self.working_dir_path,
            state_data: Listening {
                _welcome_thread: welcome_thread,
                shutdown_event: self.state_data.shutdown_event,
            },
        })
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
        let fd;
        if is_reload {
            // Safety: static read operation backed by static's safety invariant;
            fd = unsafe { RELOAD_EVENT_FOR_SIGNAL };
            if fd == -1 {
                error_in_brittle_scenario(
                    "Reload requested in an early or severely invalid state of the program, continuing.",
                );
                return;
            }
        } else {
            // Safety: static read operation backed by static's safety invariant;
            fd = unsafe { SHUTDOWN_EVENT_FOR_SIGNAL };
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
            let _ = libc::write(
                fd,
                (&raw const buf).cast::<libc::c_void>(),
                size_of_val(&buf),
            );
        }
    }
}

pub trait State {}
pub struct Uninit {}
pub struct Ready {
    welcome_socket: OwnedFd,
    shutdown_event: Arc<EventFd>,
    reload_event: Arc<EventFd>,
}
pub struct Listening {
    _welcome_thread: JoinGuard,
    shutdown_event: Arc<EventFd>,
}

impl State for Uninit {}
impl State for Ready {}
impl State for Listening {}

impl Drop for Listening {
    fn drop(&mut self) {
        let result = self.shutdown_event.write(1);
        if let Err(e) = &result {
            if thread::panicking() {
                error_in_brittle_scenario(format!("signaling welcome thread failed: {e}").as_str());
                return;
            }
            result.unwrap();
        }
    }
}
