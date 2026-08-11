/*
 * Minimal shims of certain nix functions to be used with Miri.
 * These functions do NOT provide a viable alternative!
 * They only work in this very specific scenario for debugging purposes!
 */

cfg_if::cfg_if! {
    if #[cfg(miri)] {
        use crate::constants;
        use libc::off_t;
        use libc::size_t;
        use nix::fcntl::FcntlArg;
        use nix::libc::{c_int, c_long};
        use nix::sys::memfd::MFdFlags;
        use nix::sys::mman::{MapFlags, ProtFlags};
        use nix::unistd::SysconfVar;
        use nix::{NixPath, Result};
        use std::alloc::dealloc;
        use std::alloc::{Layout, alloc};
        use std::ffi::c_void;
        use std::marker::PhantomData;
        use std::num::NonZeroUsize;
        use std::ptr::NonNull;

        pub struct OwnedFd {
            page: *mut u8
        }

        pub struct BorrowedFd<'a> {
            page: *mut u8,
            lifetime_phantom: PhantomData<&'a u8>
        }

        pub trait AsFd {
            fn as_fd(&self) -> BorrowedFd<'_>;
        }

        impl AsFd for OwnedFd {
            fn as_fd(&self) -> BorrowedFd<'_> {
                BorrowedFd {
                    page: self.page,
                    lifetime_phantom: PhantomData
                }
            }
        }

        impl AsFd for BorrowedFd<'_> {
            fn as_fd(&self) -> BorrowedFd<'_> {
                BorrowedFd {
                    page: self.page,
                    lifetime_phantom: PhantomData
                }
            }
        }

        pub fn fcntl<Fd: AsFd>(_fd: Fd, arg: FcntlArg) -> Result<c_int> {
            assert!(matches!(arg, FcntlArg::F_ADD_SEALS(_)));
            Ok(0)
        }

        pub fn memfd_create<P: NixPath + ?Sized>(_name: &P, _flags: MFdFlags) -> Result<OwnedFd> {
            Ok(OwnedFd {
                // SAFETY: PAGE_SIZE is definitely non-zero
                page: unsafe {alloc(Layout::from_size_align(constants::PAGE_SIZE, constants::PAGE_SIZE).unwrap())}
            })
        }

        pub fn ftruncate<Fd: AsFd>(_fd: Fd, len: off_t) -> Result<()> {
            assert_eq!(len, constants::PAGE_SIZE as off_t);
            Ok(())
        }

        pub fn sysconf(var: SysconfVar) -> Result<Option<c_long>> {
            assert!(matches!(var, SysconfVar::PAGE_SIZE));
            Ok(Some(constants::PAGE_SIZE as c_long))
        }

        // SAFETY: Safe. Only marked as `unsafe` to exactly match the signature
        pub unsafe fn mmap<F: AsFd>(addr: Option<NonZeroUsize>, length: NonZeroUsize, _prot: ProtFlags, _flags: MapFlags, f: F, offset: off_t) -> Result<NonNull<c_void>> {
            assert_eq!(addr, None);
            assert_eq!(length, NonZeroUsize::new(constants::PAGE_SIZE).unwrap());
            assert_eq!(offset, 0);
            Ok(NonNull::new(f.as_fd().page.cast::<c_void>()).unwrap())
        }

        // SAFETY: Memory must have been acquired through a combination of `memfd_create` and `mmap`
        // and not already been freed
        pub unsafe fn munmap(addr: NonNull<c_void>, len: size_t) -> Result<()> {
            assert_eq!(len, constants::PAGE_SIZE as size_t);
            // SAFETY: Guaranteed by the invariants of this function
            unsafe {
                dealloc(addr.as_ptr().cast::<u8>(), Layout::from_size_align(len, len).unwrap());
            }
            Ok(())
        }
    } else {
        pub use std::os::fd::{OwnedFd, BorrowedFd, AsFd};
        pub use nix::fcntl::fcntl;
        pub use nix::sys::memfd::memfd_create;
        pub use nix::unistd::{ftruncate, sysconf};
        pub use nix::sys::mman::{mmap, munmap};
    }
}
