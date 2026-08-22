use super::runtime_error::RuntimeError;
use crate::constants;
use crate::linux::welcome_thread;
use crate::util::warn;
use cfg_if::cfg_if;
use errno::{Errno, errno, set_errno};
use nix::errno::Errno as NixErrno;
use nix::libc;
use nix::sys::eventfd::{EfdFlags, EventFd};
use nix::sys::prctl::get_no_new_privs;
use nix::sys::socket::sockopt::PassCred;
use nix::sys::socket::{
    AddressFamily, SockFlag, SockType, UnixAddr, bind, setsockopt, socket,
};
use nix::unistd::{
    Gid, Uid, User, getgroups, getresgid, getresuid, setegid, seteuid, setfsgid, setfsuid, setgid,
    setuid,
};
use std::error::Error;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, mpsc};
use std::thread;
use std::thread::JoinHandle;
use garnshared::error_types::SendableError;

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
        let garn_user = User::from_name(constants::USER_NAME)?.ok_or(Box::new(RuntimeError::UserNonexistent))?;

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

        let res_gid = getresgid()?;
        if res_gid.real != garn_user.gid
            || res_gid.effective != garn_user.gid
            || res_gid.saved != garn_user.gid
        {
            return Err(Box::new(RuntimeError::RunAsWrongGroup));
        }
        if setfsgid(Gid::from_raw(u32::MAX)) != garn_user.gid {
            return Err(Box::new(RuntimeError::RunAsWrongGroup));
        }
        let groups = getgroups()?;
        if groups.contains(&Gid::from_raw(0)) {
            return Err(Box::new(RuntimeError::RunWithRootGroup));
        }

        for cap in caps::all().iter() {
            for cap_set in [caps::CapSet::Permitted, caps::CapSet::Bounding].iter() {
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

        if setuid(Uid::from_raw(0)).is_ok()
            || seteuid(Uid::from_raw(0)).is_ok()
            || {
                setfsuid(Uid::from_raw(0));
                setfsuid(Uid::from_raw(u32::MAX)) == Uid::from_raw(0)
            }
            || setgid(Gid::from_raw(0)).is_ok()
            || setegid(Gid::from_raw(0)).is_ok()
            || {
                setfsgid(Gid::from_raw(0));
                setfsgid(Gid::from_raw(u32::MAX)) == Gid::from_raw(0)
            }
        {
            return Err(Box::new(RuntimeError::CanGainPrivileges));
        }

        for cap in caps::all().iter() {
            for cap_set in [
                caps::CapSet::Bounding,
                caps::CapSet::Permitted,
                caps::CapSet::Ambient,
                caps::CapSet::Effective,
                caps::CapSet::Inheritable,
            ]
            .iter()
            {
                let mut caps_hash_set = caps::CapsHashSet::with_capacity(1);
                caps_hash_set.insert(*cap);
                if caps::set(None, *cap_set, &caps_hash_set).is_ok() {
                    return Err(Box::new(RuntimeError::CanGainPrivileges));
                }
            }
        }

        // todo: more checks?

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

        let welcome_sock_name = String::from_iter(
            [
                garnshared::constants::ABSTRACT_SOCK_NAME_PREFIX,
                garnshared::constants::WELCOME_SOCK_ABSTRACT_NAME,
            ]
            .into_iter(),
        );
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
    pub fn listen(self) -> Runtime<Listening> {
        // Ownership of the socket is moved into the thread and handed back once the threads join.
        let error_tx = self.error_tx.clone();
        let welcome_socket = self.state_data.welcome_socket;
        let shutdown_event = Arc::clone(&self.state_data.shutdown_event);
        let welcome_thread = thread::spawn(move || {
            welcome_thread::welcome_thread_main(error_tx, welcome_socket, shutdown_event)
        });

        Runtime {
            error_tx: self.error_tx,
            working_dir_path: self.working_dir_path,
            state_data: Listening {
                welcome_thread,
                shutdown_event: self.state_data.shutdown_event,
            },
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
    welcome_thread: JoinHandle<()>,
    shutdown_event: Arc<EventFd>,
}

impl State for Uninit {}
impl State for Ready {}
impl State for Listening {}

impl Drop for Listening {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown_event.write(1) {
            if thread::panicking() {
                warn(format!("signaling welcome thread failed: {}", e.to_string()).as_str());
                return;
            } else {
                Err::<usize, nix::errno::Errno>(e).unwrap();
            }
        }
    }
}
