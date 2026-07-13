use core::alloc::{AllocError, GlobalAlloc, Layout};
use core::ptr::NonNull;
use common::PAGE_SIZE;

use crate::KError;
use super::{
    pool_alloc_ffi, pool_dealloc_ffi, heap_alloc_ffi, heap_dealloc_ffi,
    map_memory_ffi, unmap_memory_ffi, allocate_memory_ffi, deallocate_memory_ffi, get_physical_address_ffi
};

#[repr(C)]
#[derive(Debug, Clone)]
pub struct PageDescriptor {
    pub num_pages: usize,
    pub start_phy_address: usize,
    pub start_virt_address: usize,
    pub flags: u8,
    pub is_mapped: bool
}

impl PageDescriptor {
    pub const VIRTUAL:  u8 = 1;       // allocate virtual + physical memory + map into current address space
    pub const USER:     u8 = 1 << 1;
    pub const NO_ALLOC: u8 = 1 << 2;  // reserve virtual space only, no physical backing
    pub const MMIO:     u8 = 1 << 3;  // uncached device-register mapping
    pub const WC:       u8 = 1 << 4;  // write-combining
}

pub trait Allocator<T> {
    fn alloc(layout: Layout) -> Result<NonNull<T>, KError>;
    unsafe fn dealloc(address: NonNull<T>, layout: Layout);
}

pub struct PoolAllocator;

#[derive(Clone, Copy)]
pub struct PoolAllocatorGlobal;

pub struct LinkedListAllocator;

impl<T> Allocator<T> for PoolAllocator {
    fn alloc(layout: Layout) -> Result<NonNull<T>, KError> {
        let mut out: *mut u8 = core::ptr::null_mut();
        let err = unsafe { pool_alloc_ffi(layout.size(), layout.align(), &mut out) };
        match err {
            KError::Success => NonNull::new(out as *mut T).ok_or(KError::OutOfMemory),
            e => Err(e),
        }
    }

    unsafe fn dealloc(address: NonNull<T>, layout: Layout) {
        unsafe {
            let _ = pool_dealloc_ffi(address.as_ptr() as *mut u8, layout.size(), layout.align());
        }
    }
}

unsafe impl core::alloc::Allocator for PoolAllocatorGlobal {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let mut out: *mut u8 = core::ptr::null_mut();
        let err = unsafe { pool_alloc_ffi(layout.size(), layout.align(), &mut out) };
        if matches!(err, KError::Success) && !out.is_null() {
            Ok(unsafe {
                NonNull::new_unchecked(core::ptr::slice_from_raw_parts_mut(out, layout.size()))
            })
        } else {
            Err(AllocError)
        }
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        unsafe {
            let _ = pool_dealloc_ffi(ptr.as_ptr(), layout.size(), layout.align());
        }
    }
}

unsafe impl GlobalAlloc for LinkedListAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut out: *mut u8 = core::ptr::null_mut();
        unsafe {
            let _ = heap_alloc_ffi(layout.size(), layout.align(), &mut out);
        }
        out
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            let _ = heap_dealloc_ffi(ptr, layout.size(), layout.align());
        }
    }
}

#[cfg(not(feature = "test-kernel"))]
#[global_allocator]
pub static GLOBAL_ALLOCATOR: LinkedListAllocator = LinkedListAllocator;

pub fn map_mmio_region(phys_addr: usize, size: usize) -> Result<*mut u8, KError> {
    let layout = Layout::from_size_align(size, PAGE_SIZE).map_err(|_| KError::InvalidArgument)?;

    let mut virt: *mut u8 = core::ptr::null_mut();
    let err = unsafe {
        allocate_memory_ffi(
            layout.size(), layout.align(),
            PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC | PageDescriptor::MMIO,
            &mut virt
        )
    };
    if !matches!(err, KError::Success) {
        return Err(err);
    }

    let err = unsafe { map_memory_ffi(phys_addr, virt as usize, size, PageDescriptor::MMIO) };
    if !matches!(err, KError::Success) {
        unsafe {
            let _ = deallocate_memory_ffi(
                virt, layout.size(), layout.align(),
                PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC
            );
        }
        return Err(err);
    }

    Ok(virt)
}

pub fn unmap_mmio_region(virt_addr: *mut u8, size: usize) -> Result<(), KError> {
    let layout = Layout::from_size_align(size, PAGE_SIZE).map_err(|_| KError::InvalidArgument)?;

    let err = unsafe { unmap_memory_ffi(virt_addr, size, PageDescriptor::MMIO) };
    if !matches!(err, KError::Success) {
        return Err(err);
    }

    let err = unsafe {
        deallocate_memory_ffi(
            virt_addr, layout.size(), layout.align(),
            PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC
        )
    };
    match err {
        KError::Success => Ok(()),
        e => Err(e)
    }
}

pub fn alloc_dma_memory(size: usize, align: usize) -> Result<(*mut u8, usize), KError> {
    let layout = Layout::from_size_align(size, align).map_err(|_| KError::InvalidArgument)?;

    let mut virt: *mut u8 = core::ptr::null_mut();
    let err = unsafe {
        allocate_memory_ffi(layout.size(), layout.align(), PageDescriptor::VIRTUAL, &mut virt)
    };
    if !matches!(err, KError::Success) {
        return Err(err);
    }

    let mut phys: usize = 0;
    if !unsafe { get_physical_address_ffi(virt as usize, PageDescriptor::VIRTUAL, &mut phys) } {
        unsafe {
            let _ = deallocate_memory_ffi(virt, layout.size(), layout.align(), PageDescriptor::VIRTUAL);
        }
        return Err(KError::InvalidArgument);
    }

    Ok((virt, phys))
}

pub fn free_dma_memory(virt_addr: *mut u8, size: usize, align: usize) -> Result<(), KError> {
    let layout = Layout::from_size_align(size, align).map_err(|_| KError::InvalidArgument)?;

    let err = unsafe {
        deallocate_memory_ffi(virt_addr, layout.size(), layout.align(), PageDescriptor::VIRTUAL)
    };
    match err {
        KError::Success => Ok(()),
        e => Err(e)
    }
}
