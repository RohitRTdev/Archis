use common::{MemoryRegion, PAGE_SIZE};
use core::marker::PhantomData;
use alloc::fmt::Display;
use alloc::vec::Vec;
use alloc::fmt::Formatter;
use crate::hal::copy_user_memory;
use crate::mem::{PageDescriptor, allocate_memory, deallocate_memory};
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
    pub fn read(&self, to: usize, len: usize, offset: usize) {
        assert!(len + offset <= self.region.size);
        if len == 0 {
            return;
        }
        
        if self.is_user {
            unsafe {
                copy_user_memory(
                    to as *mut u8, 
                    (self.region.base_address as *mut u8).add(offset),
                    len
                );
            }
        }
        else {
            unsafe {
                core::ptr::copy(
                    (self.region.base_address as *const u8).add(offset),
                    to as *mut u8,
                    len
                )
            }
        }
    }
    
    // src pointer here must be kernel memory
    pub fn write(&self, from: usize, len: usize, offset: usize) {
        assert!(len + offset <= self.region.size);
        if len == 0 {
            return;
        }

        if self.is_user {
            unsafe {
                copy_user_memory(
                    (self.region.base_address as *mut u8).add(offset),
                    from as *const u8, 
                    len
                );
            }
        }
        else {
            unsafe {
                core::ptr::copy(
                    from as *const u8,
                    (self.region.base_address as *mut u8).add(offset),
                    len
                )
            }
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

pub struct FilePath<'a> {
    path: Vec<&'a str>
}

impl<'a> From<&'a str> for FilePath<'a> {
    fn from(s: &'a str) -> Self {
        assert!(s.len() != 0 && (s.len() == 1 || !s.ends_with('/')));
        let mut path = Vec::new();
        
        // Break up the string into different parts of the file path
        let mut last_idx = 0;
        for (idx, ch) in s.chars().enumerate() {
            if ch == '/' {
                path.push(&s[last_idx..idx]);
                last_idx = idx + 1;
            }
        }

        // Prevent getting Vec = {"", ""} when path is just "/"
        if s.trim() != "/" {
            path.push(&s[last_idx..]);
        } 

        for s in path.iter().skip(1) {
            assert!(s.trim().len() != 0);
        }

        Self {
            path
        }
    }
}

impl<'a> FilePath<'a> {
    pub fn get_file_stem(&self) -> &str {
        self.path.last().unwrap()
    }
}

impl<'a> Display for FilePath<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), core::fmt::Error> {
        let _ = write!(f, "FilePath = {{");
        for s in self.path.iter().take(self.path.len() - 1) {
            let _ = write!(f, "{}, ", s);
        }
        let _ = write!(f, "{}}}", self.path.last().unwrap());
        Ok(())
    } 
}