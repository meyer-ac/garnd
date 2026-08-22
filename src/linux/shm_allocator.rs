use super::runtime_error::RuntimeError;
use crate::constants;
use crate::util::warn;
use nix::libc::off_t;
use nix::fcntl::{FcntlArg, SealFlag};
use nix::sys::memfd::MFdFlags;
use nix::sys::mman::{MapFlags, ProtFlags};
use nix::unistd::SysconfVar;
use std::collections::HashMap;
use std::ffi::c_void;
use std::num::NonZero;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::panic::{catch_unwind, UnwindSafe, resume_unwind};
use std::ptr::NonNull;
use garnshared::error_types::SendableError;
use garnshared::linux::traits::ShmSync;
use hashed_type_def::HashedTypeMethods;
use uuid::Uuid;
use super::miri::{AsFd, fcntl, memfd_create, ftruncate, sysconf, mmap, munmap};

#[derive(Copy, Clone)]
pub struct ClientResourceLocation {
    pub page: usize,
    pub offset: usize,
    pub fd: RawFd
}

struct ResourceMetadata {
    type_id: Uuid,
    page: usize,
    offset: usize,
    destructor: Box<dyn FnOnce(*mut u8) + UnwindSafe>,
}

struct Page {
    fd: OwnedFd,
    mem: NonNull<u8>,
}

pub struct ShmAllocator {
    resources: HashMap<String, ResourceMetadata>,
    page_size: usize,
    pages: Vec<Page>,
    free_ptr: usize,
}

impl ShmAllocator {
    pub fn new() -> Result<Self, SendableError> {
        let page_size = match sysconf(SysconfVar::PAGE_SIZE) {
            Ok(Some(0) | None) => return Err(Box::new(RuntimeError::GetPageSizeFailed)),
            Ok(Some(res)) => usize::try_from(res).unwrap(), // non-negative according to the Linux kernel
            Err(e) => return Err(Box::new(e)),
        };

        Ok(Self {
            resources: HashMap::new(),
            page_size,
            pages: Vec::new(),
            free_ptr: 0,
        })
    }

    pub fn move_into_shm<T: ShmSync>(
        &mut self,
        name: &str,
        resource: T,
    ) -> Result<ClientResourceLocation, SendableError> {
        if self.resources.contains_key(name) {
            return Err(Box::new(RuntimeError::ResourceNameAlreadyInUse));
        }

        if self.pages.is_empty() {
            self.create_new_page()?;
        }

        let size = size_of::<T>();
        let align = align_of::<T>();

        if align > self.page_size {
            return Err(Box::new(RuntimeError::ResourceAlignmentLargerThanPage));
        }

        let mut aligned_free_ptr = self.get_aligned_free_pointer(size, align);

        if aligned_free_ptr.is_none() {
            self.create_new_page()?;
            aligned_free_ptr = self.get_aligned_free_pointer(size, align);
            if aligned_free_ptr.is_none() {
                return Err(Box::new(RuntimeError::ResourceTooLargeForPage));
            }
        }
        let aligned_free_ptr = aligned_free_ptr.unwrap();
        self.free_ptr = aligned_free_ptr + size;

        // SAFETY: offset fits into isize, because the upper half of addresses is reserved for kernel space and
        // the whole range between the original address and the offset address belongs to the same
        // allocation (anonymous file). The address does also not wrap around the address space,
        // because the whole file is guaranteed to be in the lower half of the address space.
        let dest = unsafe {
            self.pages
                .last()
                .unwrap()
                .mem
                .as_ptr()
                .add(aligned_free_ptr)
        }
        .cast::<T>();
        // SAFETY: dest is writable (guaranteed by create_new_page)
        // and aligned (guaranteed by alignment of the free pointer)
        unsafe {
            dest.write(resource);
        }

        self.resources.insert(
            name.to_string(),
            ResourceMetadata {
                type_id: T::type_uuid(),
                page: self.pages.len() - 1,
                offset: aligned_free_ptr,
                destructor: Box::new(move |ptr| {
                    // SAFETY: validity is guaranteed by the safe class interface,
                    // alignment is guaranteed by alignment of the free pointer during creation and
                    // type coherence is guaranteed by casting ptr to the resource type T
                    drop(unsafe { ptr.cast::<T>().read() });
                }),
            },
        );

        Ok(ClientResourceLocation {
            page: self.pages.len() - 1,
            offset: aligned_free_ptr,
            fd: self.pages.last().unwrap().fd.as_raw_fd()
        })
    }

    pub fn find_resource<T: ShmSync>(&self, name: &str) -> Result<Option<ClientResourceLocation>, SendableError> {
        let Some(resource_metadata) = self.resources.get(name) else {
            return Ok(None);
        };
        if resource_metadata.type_id != T::type_uuid() {
            return Err(Box::new(RuntimeError::ResourceTypeMismatch));
        }
        Ok(Some(ClientResourceLocation {
            page: resource_metadata.page,
            offset: resource_metadata.offset,
            fd: self.pages[resource_metadata.page].fd.as_raw_fd(),
        }))
    }

    pub fn access_resource<T: ShmSync>(&self, name: &str) -> Result<Option<&T>, SendableError> {
        let Some(loc) = self.find_resource::<T>(name)? else {
            return Ok(None);
        };
        // SAFETY: raw pointer dereference: alignment is guaranteed by move_into_shm,
        // non-null and dereferenceable is guaranteed by mmap,
        // valid type is guaranteed by last if-clause,
        // no mutable references exist (because it is impossible to acquire one)
        // and safe Rust guarantees that this references will not be mutated.
        // Since there is no way to deallocate or modify a resource, the reference is valid for as
        // long as self is alive.
        // add: offset fits into isize, because the upper half of addresses is reserved for kernel space and
        // the whole range between the original address and the offset address belongs to the same
        // allocation (anonymous file). The address does also not wrap around the address space,
        // because the whole file is guaranteed to be in the lower half of the address space.
        Ok(Some(unsafe {
            &*self.pages[loc.page].mem.as_ptr()
                .add(loc.offset)
                .cast::<T>()
        }))
    }

    fn create_new_page(&mut self) -> Result<(), SendableError> {
        let shm_fd = match memfd_create(
            constants::SHM_FILE_NAME,
            MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING,
        ) {
            Ok(res) => res,
            Err(e) => return Err(Box::new(e)),
        };

        if let Err(e) = ftruncate(shm_fd.as_fd(), off_t::try_from(self.page_size).unwrap()) {
            return Err(Box::new(e));
        }

        if let Err(e) = fcntl(
            shm_fd.as_fd(),
            FcntlArg::F_ADD_SEALS(
                SealFlag::F_SEAL_SEAL | SealFlag::F_SEAL_SHRINK | SealFlag::F_SEAL_GROW,
            ),
        ) {
            return Err(Box::new(e));
        }

        // SAFETY: length is guaranteed to be non-zero in Self::new(),
        // prot and flags are only passed valid flags,
        // offset is trivially a multiple of the system's page size and
        // addr is omitted.
        match unsafe {
            mmap(
                None,
                NonZero::new(self.page_size).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                shm_fd.as_fd(),
                0,
            )
        } {
            Ok(res) => {
                self.pages.push(Page {
                    fd: shm_fd,
                    mem: res.cast::<u8>(),
                });
            }
            Err(e) => return Err(Box::new(e)),
        }

        self.free_ptr = 0;

        Ok(())
    }

    fn get_aligned_free_pointer(&self, size: usize, align: usize) -> Option<usize> {
        let aligned_free_ptr = if self.free_ptr.is_multiple_of(align) {
            self.free_ptr
        } else {
            self.free_ptr + (align - self.free_ptr % align)
        };
        if aligned_free_ptr + size > self.page_size {
            return None;
        }
        Some(aligned_free_ptr)
    }
}

impl Drop for ShmAllocator {
    fn drop(&mut self) {
        // Catch the first panic and later resume it, so that everything looks normal from the outside,
        // but free all the shared resources first
        let mut first_panic = None;
        for (_, resource_metadata) in self.resources.drain() {
            if let Err(e) = catch_unwind(|| {
                (resource_metadata.destructor)(
                    // SAFETY: offset fits into isize, because the upper half of addresses is reserved for kernel space and
                    // the whole range between the original address and the offset address belongs to the same
                    // allocation (anonymous file). The address does also not wrap around the address space,
                    // because the whole file is guaranteed to be in the lower half of the address space.
                    unsafe {
                        self.pages[resource_metadata.page]
                            .mem
                            .as_ptr()
                            .add(resource_metadata.offset)
                    },
                );
            }) {
                first_panic = first_panic.or(Some(e));
            }
        }
        for page in &self.pages {
            // SAFETY: addr being a multiple of the page size is guaranteed by mmap, which
            // aligns the memory to page boundaries
            if let Err(e) = unsafe { munmap(page.mem.cast::<c_void>(), self.page_size) } {
                warn(format!("unmapping of shared memory failed: {}", e).as_str());
            }
        }
        if let Some(first_panic) = first_panic {
            if std::thread::panicking() {
                warn("ShmAllocator panicked while destructing");
            } else {
                resume_unwind(first_panic);
            }
        }
    }
}
