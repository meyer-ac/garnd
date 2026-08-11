use super::runtime_error::RuntimeError;
use crate::constants;
use crate::util::warn;
use cfg_if::cfg_if;
use errno::{Errno, errno, set_errno};
use nix::libc;
use nix::sys::prctl::get_no_new_privs;
use nix::unistd::{
    Gid, Uid, User, getgroups, getresgid, getresuid, setegid, seteuid, setfsgid, setfsuid, setgid,
    setuid,
};
use std::error::Error;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;

pub struct Runtime<S: State> {
    _state: PhantomData<S>,
    tx: mpsc::Sender<RuntimeError>,
    working_dir_path: PathBuf,
    welcome_sock_rel_path: PathBuf,
    welcome_thread: Option<JoinHandle<()>>,
}

impl Runtime<Uninit> {
    pub fn new(working_dir_name: Option<&str>) -> (Self, mpsc::Receiver<RuntimeError>) {
        let (tx, rx) = mpsc::channel::<RuntimeError>();
        let working_dir_path =
            Path::new(working_dir_name.unwrap_or(constants::WORKING_DIR)).to_path_buf();
        let welcome_sock_rel_path = Path::new(constants::WELCOME_SOCK_NAME).to_path_buf();

        (
            Self {
                _state: PhantomData,
                tx,
                working_dir_path,
                welcome_sock_rel_path,
                welcome_thread: None,
            },
            rx,
        )
    }

    pub fn init(self) -> Result<Runtime<Ready>, Box<dyn Error>> {
        Self::check_privileges()?;

        Ok(Runtime {
            _state: PhantomData,
            tx: self.tx,
            working_dir_path: self.working_dir_path,
            welcome_sock_rel_path: self.welcome_sock_rel_path,
            welcome_thread: None,
        })
    }

    #[allow(clippy::similar_names)] // uid and gid being similar is fine
    fn check_privileges() -> Result<(), Box<dyn Error>> {
        cfg_if! {
            if #[cfg(debug_assertions)] {
                warn("privilege checks are disabled in debug mode");
                return Ok(());
            }
        }
        #[allow(unreachable_code)] // Only unreachable in debug mode, which is intended
        let garn_user = match User::from_name(constants::USER_NAME) {
            Ok(Some(res)) => res,
            Ok(None) => return Err(Box::new(RuntimeError::UserNonexistent)),
            Err(e) => return Err(Box::new(e)),
        };

        let res_uid = match getresuid() {
            Ok(res) => res,
            Err(e) => return Err(Box::new(e)),
        };
        if res_uid.real != garn_user.uid
            || res_uid.effective != garn_user.uid
            || res_uid.saved != garn_user.uid
        {
            return Err(Box::new(RuntimeError::RunAsWrongUser));
        }
        if setfsuid(Uid::from_raw(u32::MAX)) != garn_user.uid {
            return Err(Box::new(RuntimeError::RunAsWrongUser));
        }

        let res_gid = match getresgid() {
            Ok(res) => res,
            Err(e) => return Err(Box::new(e)),
        };
        if res_gid.real != garn_user.gid
            || res_gid.effective != garn_user.gid
            || res_gid.saved != garn_user.gid
        {
            return Err(Box::new(RuntimeError::RunAsWrongGroup));
        }
        if setfsgid(Gid::from_raw(u32::MAX)) != garn_user.gid {
            return Err(Box::new(RuntimeError::RunAsWrongGroup));
        }
        let groups = match getgroups() {
            Ok(res) => res,
            Err(e) => return Err(Box::new(e)),
        };
        if groups.contains(&Gid::from_raw(0)) {
            return Err(Box::new(RuntimeError::RunWithRootGroup));
        }

        for cap in caps::all().iter() {
            for cap_set in [caps::CapSet::Permitted, caps::CapSet::Bounding].iter() {
                let has_cap = match caps::has_cap(None, *cap_set, *cap) {
                    Ok(res) => res,
                    Err(e) => return Err(Box::new(e)),
                };
                if has_cap {
                    return Err(Box::new(RuntimeError::RunWithCapabilities));
                }
            }
        }

        let no_new_privs = match get_no_new_privs() {
            Ok(res) => res,
            Err(e) => return Err(Box::new(e)),
        };
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
}

impl Runtime<Ready> {
    pub fn listen(self) -> Runtime<Listening> {
        let welcome_sock_path = self
            .working_dir_path
            .join(self.welcome_sock_rel_path.as_path());
        let thread_tx = self.tx.clone();
        let welcome_thread = thread::spawn(move || {
            let welcome_sock_path = welcome_sock_path;
            let tx = thread_tx;
        });

        Runtime {
            _state: PhantomData,
            tx: self.tx,
            working_dir_path: self.working_dir_path,
            welcome_sock_rel_path: self.welcome_sock_rel_path,
            welcome_thread: None,
        }
    }
}

pub trait State {}
pub enum Uninit {}
pub enum Ready {}
pub enum Listening {}

impl State for Uninit {}
impl State for Ready {}
impl State for Listening {}
