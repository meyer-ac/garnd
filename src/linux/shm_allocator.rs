use super::runtime_error::RuntimeError;
use crate::constants;
use crate::util::warn;
use garnshared::error_types::SendableError;
use garnshared::linux::traits::ShmCompatible;
use hashed_type_def::HashedTypeMethods;
use nix::fcntl::{FcntlArg, SealFlag, fcntl};
use nix::libc::off_t;
use nix::sys::memfd::{MFdFlags, memfd_create};
use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap};
use nix::unistd::{SysconfVar, ftruncate, sysconf};
use std::any;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::num::NonZero;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::panic::{UnwindSafe, catch_unwind, resume_unwind};
use std::pin::Pin;
use std::ptr::NonNull;
use uuid::Uuid;

#[derive(Copy, Clone)]
pub struct ClientResourceLocation {
    pub page: usize,
    pub offset: usize,
    pub fd: RawFd,
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

    pub fn construct_in_shm<T, F>(
        &mut self,
        name: &str,
        placement_constructor: F,
    ) -> Result<ClientResourceLocation, SendableError>
    where
        T: ShmCompatible,
        F: FnOnce(Pin<&mut MaybeUninit<T>>) -> Result<(), SendableError>,
    {
        if self.resources.contains_key(name) {
            return Err(Box::new(RuntimeError::ResourceNameAlreadyInUse {
                resource_name: name.to_owned(),
            }));
        }

        let size = size_of::<T>();
        let align = align_of::<T>();

        if size > self.page_size {
            return Err(Box::new(RuntimeError::ResourceTooLargeForPage {
                page_size: self.page_size,
                size,
            }));
        }

        if align > self.page_size {
            return Err(Box::new(RuntimeError::ResourceAlignmentLargerThanPage {
                page_size: self.page_size,
                alignment: align,
            }));
        }

        if self.pages.is_empty() {
            self.create_new_page()?;
        }

        let mut aligned_free_ptr = self.get_aligned_free_pointer(size, align);

        if aligned_free_ptr.is_none() {
            self.create_new_page()?;
            aligned_free_ptr = self.get_aligned_free_pointer(size, align);
        }
        let aligned_free_ptr = aligned_free_ptr.unwrap();
        self.free_ptr = aligned_free_ptr + size;

        // SAFETY: add: offset fits into isize, because the upper half of addresses is reserved for kernel space and
        // the whole range between the original address and the offset address belongs to the same
        // allocation (anonymous file). The address does also not wrap around the address space,
        // because the whole file is guaranteed to be in the lower half of the address space.
        // dereferencing: the raw pointer points to a well-aligned, right-sized slot. The slot
        // is "initialized" in the sense that the MaybeUninit wrapper allows it to contain
        // uninitialized memory.
        let dest = unsafe {
            &mut *self
                .pages
                .last_mut()
                .unwrap()
                .mem
                .as_ptr()
                .add(aligned_free_ptr)
                .cast::<MaybeUninit<T>>()
        };

        /*
        // SAFETY: dest is writable (guaranteed by create_new_page)
        // and aligned (guaranteed by alignment of the free pointer)
        unsafe {
            dest.write(resource);
        }
         */

        // SAFETY: ShmAllocator does not provide a way to move the pinned value through its safe
        // interface
        placement_constructor(unsafe { Pin::new_unchecked(dest) })?;

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
            fd: self.pages.last().unwrap().fd.as_raw_fd(),
        })
    }

    pub fn find_resource<T: ShmCompatible>(
        &self,
        name: &str,
    ) -> Result<Option<ClientResourceLocation>, SendableError> {
        let Some(resource_metadata) = self.resources.get(name) else {
            return Ok(None);
        };
        if resource_metadata.type_id != T::type_uuid() {
            return Err(Box::new(RuntimeError::ResourceTypeMismatch {
                requested_type: any::type_name::<T>(),
                resource_type: "<unavailable at runtime>",
            }));
        }
        Ok(Some(ClientResourceLocation {
            page: resource_metadata.page,
            offset: resource_metadata.offset,
            fd: self.pages[resource_metadata.page].fd.as_raw_fd(),
        }))
    }

    fn create_new_page(&mut self) -> Result<(), SendableError> {
        let shm_fd = memfd_create(
            constants::SHM_FILE_NAME,
            MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING,
        )?;

        ftruncate(shm_fd.as_fd(), off_t::try_from(self.page_size)?)?;

        fcntl(
            shm_fd.as_fd(),
            FcntlArg::F_ADD_SEALS(
                SealFlag::F_SEAL_SEAL | SealFlag::F_SEAL_SHRINK | SealFlag::F_SEAL_GROW,
            ),
        )?;

        // SAFETY: length is guaranteed to be non-zero in Self::new(),
        // prot and flags are only passed valid flags,
        // offset is trivially a multiple of the system's page size and
        // addr is omitted.
        unsafe {
            mmap(
                None,
                NonZero::new(self.page_size).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                shm_fd.as_fd(),
                0,
            )
        }
        .map(|res| {
            self.free_ptr = 0;
            self.pages.push(Page {
                fd: shm_fd,
                mem: res.cast::<u8>(),
            });
        })
        .map_err(Box::from)
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
                warn(format!("unmapping of shared memory failed: {e}").as_str());
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
