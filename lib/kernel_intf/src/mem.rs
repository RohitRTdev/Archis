use core::alloc::{AllocError, GlobalAlloc, Layout};
use core::ptr::NonNull;
use crate::KError;
use super::{pool_alloc_ffi, pool_dealloc_ffi, heap_alloc_ffi, heap_dealloc_ffi};
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
