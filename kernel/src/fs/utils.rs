use common::{MemoryRegion, PAGE_SIZE};
use core::marker::PhantomData;
use crate::mem::{self, PageDescriptor, allocate_memory, deallocate_memory};
use kernel_intf::KError;
use core::alloc::Layout;

pub struct FileBuffer {
    region: MemoryRegion,
    is_user: bool,
    own: bool,

    // Send trait is unsafe here, because if the buffer refers 
    // to user memory, it is only valid in the current process context
    _nosend: PhantomData<*const ()> 
}

impl FileBuffer {
    pub fn new(size: usize, is_user: bool) -> Result<Self, KError> {
        let base_address = allocate_memory(
            Layout::from_size_align(size, PAGE_SIZE).unwrap(),
        PageDescriptor::VIRTUAL | (if is_user {PageDescriptor::USER} else {0})
        )?.addr();

        Ok(
            Self {
                region: MemoryRegion {
                    base_address,
                    size
                },
                is_user,
                own: true,
                _nosend: PhantomData
            }
        )
    }

    pub fn from(base_address: usize, size: usize, is_user: bool) -> Self {
        Self {
            region: MemoryRegion { 
                base_address, 
                size 
            },
            is_user,
            own: false,
            _nosend: PhantomData
        }
    }

    // dest pointer here must be kernel memory
    pub fn read(&self, to: usize, len: usize, offset: usize) -> Result<(), KError> {
        assert!(len + offset <= self.region.size);
        if len == 0 {
            return Ok(());
        }

        if self.is_user {
            mem::copy_from_user(
                to as *mut u8,
                self.region.base_address + offset,
                len
            )
        }
        else {
            unsafe {
                core::ptr::copy(
                    (self.region.base_address as *const u8).add(offset),
                    to as *mut u8,
                    len
                )
            }
            Ok(())
        }
    }

    // src pointer here must be kernel memory
    pub fn write(&self, from: usize, len: usize, offset: usize) -> Result<(), KError> {
        assert!(len + offset <= self.region.size);
        if len == 0 {
            return Ok(());
        }

        if self.is_user {
            mem::copy_to_user(
                self.region.base_address + offset,
                from as *const u8,
                len
            )
        }
        else {
            unsafe {
                core::ptr::copy(
                    from as *const u8,
                    (self.region.base_address as *mut u8).add(offset),
                    len
                )
            }
            Ok(())
        }
    }

    pub fn len(&self) -> usize {
        self.region.size
    }

    pub fn as_slice<'a>(&'a self) -> &'a [u8] {
        unsafe {
            core::slice::from_raw_parts(
                self.region.base_address as *const u8,
                self.len()
            )
        }
    }
}

impl Drop for FileBuffer {
    fn drop(&mut self) {
        if !self.own {
            return;
        }

        deallocate_memory(
            self.region.base_address as *mut u8,
            Layout::from_size_align(
                self.region.size,
                PAGE_SIZE
            ).unwrap(),
            PageDescriptor::VIRTUAL | (if self.is_user {PageDescriptor::USER} else {0})
        ).expect("Filebuffer memory deallocation failed!");
    }
}

