use super::runtime_error::RuntimeError;
use crate::constants;
use crate::join_guard::JoinGuard;
use crate::linux::welcome_thread;
use crate::util::warn;
use cfg_if::cfg_if;
use errno::{Errno, errno, set_errno};
use garnshared::error_types::SendableError;
use nix::errno::Errno as NixErrno;
use nix::libc;
use nix::libc::_exit;
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::prctl::get_no_new_privs;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, Signal, sigaction};
use nix::sys::socket::sockopt::PassCred;
use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, bind, setsockopt, socket};
use nix::sys::stat::{stat, Mode};
use nix::unistd::{Gid, Group, Uid, User, getgroups, getresgid, getresuid, setfsgid, setfsuid};
use std::ffi::c_int;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, mpsc};
use std::{fs, thread};
use std::fs::File;

/// Only used for the termination signal handler, NOWHERE ELSE!
/// # SAFETY
/// Only written to once before the signal handler is installed.
static mut SHUTDOWN_EVENT_FOR_SIGNAL: c_int = -1;

pub struct Runtime<S: State> {
    error_tx: Sender<SendableError>,
    working_dir_path: PathBuf,
    state_data: S,
}

impl Runtime<Uninit> {
    pub fn new(working_dir_name: Option<&str>) -> (Self, mpsc::Receiver<SendableError>) {
        let (tx, rx) = mpsc::channel::<SendableError>();
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

    pub fn init(self) -> Result<Runtime<Ready>, SendableError> {
        Self::check_privileges()?;

        self.setup_working_dir()?;

        let (welcome_socket, shutdown_event) = Self::setup_socket()?;

        Ok(Runtime {
            error_tx: self.error_tx,
            working_dir_path: self.working_dir_path,
            state_data: Ready {
                welcome_socket,
                shutdown_event: Arc::new(shutdown_event),
            },
        })
    }

    #[allow(clippy::similar_names)] // uid and gid being similar is fine
    fn check_privileges() -> Result<(), SendableError> {
        cfg_if! {
            if #[cfg(debug_assertions)] {
                warn("privilege checks are disabled in debug mode");
                return Ok(());
            }
        }
        #[allow(unreachable_code)] // Only unreachable in debug mode, which is intended
        let garn_user = User::from_name(constants::USER_NAME)?
            .ok_or(Box::new(RuntimeError::UserNonexistent))?;
        let res_uid = getresuid()?;
        if res_uid.real != garn_user.uid
            || res_uid.effective != garn_user.uid
            || res_uid.saved != garn_user.uid
        {
            return Err(Box::new(RuntimeError::RunAsWrongUser));
        }
        if setfsuid(Uid::from_raw(u32::MAX)) != garn_user.uid {
            return Err(Box::new(RuntimeError::RunAsWrongUser));
        }

        let garn_group = Group::from_name(constants::GROUP_NAME)?
            .ok_or(Box::new(RuntimeError::GroupNonexistent))?;
        let res_gid = getresgid()?;
        if res_gid.real != garn_group.gid
            || res_gid.effective != garn_group.gid
            || res_gid.saved != garn_group.gid
        {
            return Err(Box::new(RuntimeError::RunAsWrongGroup));
        }
        if setfsgid(Gid::from_raw(u32::MAX)) != garn_group.gid {
            return Err(Box::new(RuntimeError::RunAsWrongGroup));
        }
        let groups = getgroups()?;
        if groups.contains(&Gid::from_raw(0)) {
            return Err(Box::new(RuntimeError::RunWithRootGroup));
        }

        for cap in &caps::all() {
            for cap_set in &[caps::CapSet::Permitted, caps::CapSet::Bounding] {
                let has_cap = caps::has_cap(None, *cap_set, *cap)?;
                if has_cap {
                    return Err(Box::new(RuntimeError::RunWithCapabilities));
                }
            }
        }

        let no_new_privs = get_no_new_privs()?;
        if !no_new_privs {
            return Err(Box::new(RuntimeError::MayObtainNewPrivileges));
        }

        set_errno(Errno(0));
        // SAFETY: We pass a valid value to option and zeroes everywhere else, hence the call is safe.
        let secure_bits = unsafe { libc::prctl(libc::PR_GET_SECUREBITS, 0, 0, 0, 0) };
        if secure_bits == -1 {
            return Err(Box::new(std::io::Error::from_raw_os_error(errno().0)));
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
            return Err(Box::new(RuntimeError::SecureBitsNotSet));
        }

        Ok(())
    }

    fn setup_working_dir(&self) -> Result<(), SendableError> {
        let working_dir_str = self
            .working_dir_path
            .clone()
            .into_os_string()
            .into_string()
            .map_err(|_| Box::new(RuntimeError::WorkingDirPathInvalidString))?;
        if !fs::exists(&self.working_dir_path)? {
            return Err(Box::new(RuntimeError::WorkingDirNonexistent {
                working_dir: working_dir_str,
            }));
        }
        if !fs::metadata(&self.working_dir_path)?.is_dir() {
            return Err(Box::new(RuntimeError::WorkingDirNotADirectory {
                working_dir: working_dir_str,
            }));
        }
        let stats = stat(&self.working_dir_path)?;

        cfg_if! {
            if #[cfg(debug_assertions)] {
                return Ok(())
            }
        }

        // Verify owner
        let garn_user = User::from_name(constants::USER_NAME)?
            .ok_or(Box::new(RuntimeError::UserNonexistent))?;
        let garn_group = Group::from_name(constants::GROUP_NAME)?
            .ok_or(Box::new(RuntimeError::GroupNonexistent))?;
        let owner_user = User::from_uid(Uid::from_raw(stats.st_uid))?.unwrap();
        let owner_group = Group::from_gid(Gid::from_raw(stats.st_gid))?.unwrap();
        if owner_user.uid != garn_user.uid {
            return Err(Box::new(RuntimeError::WorkingDirOwnedByWrongUser {
                working_dir: working_dir_str,
                owner: owner_user.name,
            }));
        }
        if owner_group.gid != garn_group.gid {
            return Err(Box::new(RuntimeError::WorkingDirOwnedByWrongGroup {
                working_dir: working_dir_str,
                owner: owner_user.name,
            }));
        }

        // Verify permissions
        let mode = Mode::from_bits_truncate(stats.st_mode);
        if !(mode.contains(Mode::S_IRWXU | Mode::S_IRGRP | Mode::S_IXGRP | Mode::S_IROTH | Mode::S_IXOTH) && !mode.contains(Mode::S_IWGRP) && !mode.contains(Mode::S_IWOTH)) {
            return Err(Box::new(RuntimeError::WorkingDirWrongPermissions {working_dir: working_dir_str, permissions: "rwxr-xr-x"}))
        }
        if mode.contains(Mode::S_ISUID) {
            return Err(Box::new(RuntimeError::WorkingDirSetUidBitSet {working_dir: working_dir_str}));
        }
        if mode.contains(Mode::S_ISGID) {
            return Err(Box::new(RuntimeError::WorkingDirSetGidBitSet {working_dir: working_dir_str}));
        }
        if mode.contains(Mode::S_ISVTX) {
            return Err(Box::new(RuntimeError::WorkingDirStickyBitSet {working_dir: working_dir_str}));
        }
        
        Ok(())
    }

    fn setup_socket() -> Result<(OwnedFd, EventFd), SendableError> {
        let welcome_socket = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        )?;

        // todo: re-evaluate if this is necessary
        if let Err(e) = setsockopt(&welcome_socket.as_fd(), PassCred, &true) {
            return Err(Box::new(e));
        }

        let welcome_sock_name = [
            garnshared::constants::ABSTRACT_SOCK_NAME_PREFIX,
            garnshared::constants::WELCOME_SOCK_ABSTRACT_NAME,
        ]
        .into_iter()
        .collect::<String>();
        let addr = match UnixAddr::new_abstract(welcome_sock_name.as_bytes()) {
            Ok(res) => res,
            Err(e) => return Err(Box::new(e)),
        };

        if let Err(e) = bind(welcome_socket.as_raw_fd(), &addr) {
            return match e {
                NixErrno::EADDRINUSE => Err(Box::new(RuntimeError::ServiceAlreadyRunning)),
                e => Err(Box::new(e)),
            };
        }

        let shutdown_event = match EventFd::from_value_and_flags(
            0,
            EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK,
        ) {
            Ok(res) => res,
            Err(e) => return Err(Box::new(e)),
        };

        Ok((welcome_socket, shutdown_event))
    }
}

impl Runtime<Ready> {
    pub fn create_log_file(&self, file_name: &str) -> Result<File, SendableError> {
        File::create(self.working_dir_path.join(file_name)).map_err(Box::from)
    }

    pub fn listen(self) -> Result<Runtime<Listening>, SendableError> {
        // Setup signal handler for graceful shutdown
        // Safety: This is the only write to the static before the signal handler is installed.
        unsafe {
            SHUTDOWN_EVENT_FOR_SIGNAL = self.state_data.shutdown_event.as_raw_fd();
        }
        // Safety: The signal handler is async safe.
        unsafe {
            sigaction(
                Signal::SIGTERM,
                &SigAction::new(
                    SigHandler::Handler(Self::termination_signal_handler),
                    SaFlags::SA_RESTART,
                    Signal::SIGTERM | Signal::SIGINT,
                ),
            )
        }?;
        unsafe {
            sigaction(
                Signal::SIGINT,
                &SigAction::new(
                    SigHandler::Handler(Self::termination_signal_handler),
                    SaFlags::SA_RESTART,
                    Signal::SIGTERM | Signal::SIGINT,
                ),
            )
        }?;

        // Ownership of the socket is moved into the thread and handed back once the threads join.
        let error_tx = self.error_tx.clone();
        let welcome_socket = self.state_data.welcome_socket;
        let shutdown_event = Arc::clone(&self.state_data.shutdown_event);
        //let welcome_thread = thread::spawn(move || {
        //    welcome_thread::welcome_thread_main(error_tx, welcome_socket, shutdown_event)
        //});
        let welcome_thread = JoinGuard::from(thread::Builder::new().spawn(move || {
            welcome_thread::welcome_thread_main(&error_tx, welcome_socket, &shutdown_event);
        })?);

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
        // Safety: backed by static's safety invariant
        if unsafe { SHUTDOWN_EVENT_FOR_SIGNAL } == -1 {
            warn(
                "Termination requested in an early or invalid state of the program, exiting immediately.",
            );
            // Safety: Potentially ill-formed program states are irrelevant here, because we exit immediately anyway
            unsafe {
                _exit(-1);
            }
        } else {
            let buf: i64 = 1;
            // Safety: static read operation backed by static's safety invariant;
            // a write operation to an invalid fd cannot cause UB;
            // the value written to it is exactly 8 bytes;
            // `write` is async safe.
            unsafe {
                let _ = libc::write(
                    SHUTDOWN_EVENT_FOR_SIGNAL,
                    (&raw const buf).cast::<libc::c_void>(),
                    size_of_val(&buf),
                );
            }
        }
    }
}

pub trait State {}
pub struct Uninit {}
pub struct Ready {
    welcome_socket: OwnedFd,
    shutdown_event: Arc<EventFd>,
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
                warn(format!("signaling welcome thread failed: {e}").as_str());
                return;
            }
            result.unwrap();
        }
    }
}
