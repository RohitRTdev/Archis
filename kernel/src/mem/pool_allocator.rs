use core::ptr::NonNull;
use crate::mem::PageDescriptor;
use crate::sync::Spinlock;
use kernel_intf::KError;
use kernel_intf::debug;
use crate::mem::FixedList;
use kernel_intf::list::List;
use super::fixed_allocator::Regions::*;
use super::allocate_memory;
use common::PAGE_SIZE;

const ALLOCATION_UNIT: usize = PAGE_SIZE * 2;

// Represents a single pool for a specific block size.
struct Pool {
    block_size: usize,
    free_list: Option<NonNull<FreeBlock>>
}

unsafe impl Send for Pool{}

impl Pool {
    fn new(block_size: usize) -> Self {
        Pool {
            block_size,
            free_list: None
        }
    }

#[cfg(debug_assertions)]
    fn print(&self, idx: usize) {
        debug!("Pool idx:{}, size: {}", idx, self.block_size);

        let mut cur_block = self.free_list;
        while cur_block.is_some() {
            debug!("Block: {:#X}", cur_block.unwrap().as_ptr().addr());

            cur_block = unsafe {
                (*cur_block.unwrap().as_ptr()).next
            };
        }
    }
}

// Linked list to track free slots.
#[repr(C)]
struct FreeBlock {
    next: Option<NonNull<FreeBlock>>
}

impl FreeBlock {
    fn set_next(&mut self, next: Option<NonNull<FreeBlock>>) {
        self.next = next;
    }
}

// Maintains a list of pools for different block sizes.
struct PoolControlBlock {
    pools: FixedList<Pool, {Region4 as usize}>
}

impl PoolControlBlock {
    fn find_pool_mut(&mut self, block_size: usize) -> Option<&mut Pool> {
        self.pools.iter_mut().find(|pool| pool.block_size == block_size)
        .and_then(|item| {
            Some(&mut **item)
        })
    }

    fn add_pool(&mut self, block_size: usize) -> Result<&mut Pool, KError> {
        let pool = Pool::new(block_size);
        self.pools.add_node(pool).map_err(|_| {
            KError::OutOfMemory
        })?;

        Ok(self.find_pool_mut(block_size).unwrap())
    }

#[cfg(debug_assertions)]
    fn print_pool(&self) {
        debug!("===Printing pools===");
        for (idx, pool) in self.pools.iter().enumerate() {
            pool.print(idx);
        }
    }
}

static POOL_CB: Spinlock<PoolControlBlock> = Spinlock::new(PoolControlBlock {
    pools: List::new()
});

// Push a range of slots as free blocks into the pool's free list
fn push_free_blocks(pool: &mut Pool, base: *mut u8, slots: usize, block_size: usize) {
    for i in 0..slots {
        let slot_ptr = unsafe { base.add(i * block_size) as *mut FreeBlock };
        unsafe {
            (*slot_ptr).set_next(pool.free_list);
            pool.free_list = Some(NonNull::new_unchecked(slot_ptr));
        }
    }
}

fn allocate_block(size: usize, align: usize) -> Result<NonNull<u8>, KError> {
    //kernel_intf::debug!("Requesting pool allocation, size={}, align={}", size, align);
    let _ = align;
    let block_size = size;
    let mut cb = POOL_CB.lock();

    // Find or create the pool for this block size
    let pool = match cb.find_pool_mut(block_size) {
        Some(pool) => pool,
        None => cb.add_pool(block_size)?,
    };

    // If free_list is not empty, pop and return
    if let Some(free_block) = pool.free_list {
        let next = unsafe { (*free_block.as_ptr()).next };
        pool.free_list = next;

        return Ok(unsafe { NonNull::new_unchecked(free_block.as_ptr() as *mut u8) });
    }

    // No free slots, allocate a new block and push all slots to free_list
    let slots_per_block = ALLOCATION_UNIT / block_size;
    let base = allocate_memory_raw(ALLOCATION_UNIT, PAGE_SIZE, PageDescriptor::VIRTUAL)?;

    // Push all slots to free_list
    push_free_blocks(pool, base, slots_per_block, block_size);

    // Pop one for this allocation
    if let Some(free_block) = pool.free_list {
        let next = unsafe { (*free_block.as_ptr()).next };
        pool.free_list = next;

        return Ok(unsafe { NonNull::new_unchecked(free_block.as_ptr() as *mut u8) });
    }

    Err(KError::OutOfMemory)
}

unsafe fn deallocate_block(ptr: NonNull<u8>, size: usize, _align: usize) {
    //kernel_intf::debug!("Requesting pool deallocation on addr={:#X}, size={}, align={}", ptr.as_ptr().addr(), size, _align);
    let block_size = size;
    let mut cb = POOL_CB.lock();

    // Find the pool for this block size and add the released block back to head of free_list
    if let Some(pool) = cb.find_pool_mut(block_size) {
        let free_ptr = ptr.as_ptr() as *mut FreeBlock;
        unsafe {
            (*free_ptr).set_next(pool.free_list);
            pool.free_list = Some(NonNull::new_unchecked(free_ptr));
        }
    }
    else {
        debug_assert!(false, "pool_allocator -> dealloc called for unknown pointer :{:#X} and size:{}",
        ptr.as_ptr() as usize, size);
    }
}

fn allocate_memory_raw(size: usize, align: usize, flags: u8) -> Result<*mut u8, KError> {
    allocate_memory(core::alloc::Layout::from_size_align(size, align).unwrap(), flags)
}

#[unsafe(no_mangle)]
extern "C" fn pool_alloc_ffi(size: usize, align: usize, out: *mut *mut u8) -> KError {
    assert!(size >= size_of::<FreeBlock>() && size <= PAGE_SIZE
        && align <= size && size % align == 0);

    match allocate_block(size, align) {
        Ok(ptr) => {
            unsafe { *out = ptr.as_ptr(); }
            KError::Success
        }
        Err(e) => {
            unsafe { *out = core::ptr::null_mut(); }
            e
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn pool_dealloc_ffi(ptr: *mut u8, size: usize, align: usize) -> KError {
    assert!(size >= size_of::<FreeBlock>() && size <= PAGE_SIZE
        && align <= size && size % align == 0);

    match NonNull::new(ptr) {
        Some(ptr) => {
            unsafe { deallocate_block(ptr, size, align); }
            KError::Success
        }
        None => KError::InvalidArgument
    }
}
