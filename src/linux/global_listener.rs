use std::path::{Path, PathBuf};
use rustix::net::{socket_with, AddressFamily, SocketType, SocketFlags, bind, SocketAddrUnix};

pub struct GlobalListener {
    sock_path: PathBuf
}

impl GlobalListener {
    pub fn new(working_dir: &str, sock_name: &str) -> Self {
        Self {
            sock_path: Path::new(working_dir).join(sock_name)
        }
    }

    pub fn listen(&self) {
        let sock = socket_with(
            AddressFamily::UNIX,
            SocketType::DGRAM,
            SocketFlags::empty(),
            None
        ).unwrap();

        let addr = SocketAddrUnix::new(
            self.sock_path.as_path()
        ).unwrap();

        bind(
            sock,
            &addr
        ).unwrap();
    }
}