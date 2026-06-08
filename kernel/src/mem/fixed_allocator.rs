#![allow(static_mut_refs)]

use core::marker::PhantomData;
use core::mem;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::alloc::Layout;
use common::{ceil_div, PAGE_SIZE};
use kernel_intf::KError;
use kernel_intf::mem::Allocator;
use kernel_intf::list::{List, ListNode};
use crate::sync::Spinlock;
use kernel_intf::info;

pub type FixedList<T, const REGION: usize> = List<T, FixedAllocator<ListNode<T>, REGION>>;

#[repr(usize)]
pub enum Regions {
    Region0,
    Region1,
    Region2,
    Region3,
    Region4
}

const TOTAL_REGIONS: usize = 5;
pub const BOOT_REGIONS: [usize; TOTAL_REGIONS] = [40 * PAGE_SIZE, 4 * PAGE_SIZE, PAGE_SIZE, 10 * PAGE_SIZE, PAGE_SIZE];
const TOTAL_BOOT_MEMORY: usize = BOOT_REGIONS[0] + BOOT_REGIONS[1] + BOOT_REGIONS[2] + BOOT_REGIONS[3]
 + BOOT_REGIONS[4];

// Here we simply divide given memory into slots each of size 8 bytes
// 8 is chosen to represent an average DS size
pub const MIN_SLOT_SIZE: usize = 8;
pub const BITMAP_SIZE: usize = (TOTAL_BOOT_MEMORY / MIN_SLOT_SIZE) >> 3;

// Wrapper required to force alignment constraint
#[repr(C)]
#[cfg_attr(target_arch="x86_64", repr(align(4096)))]
struct HeapWrapper {
    buffer: [u8; TOTAL_BOOT_MEMORY],
    bitmap: [u8; BITMAP_SIZE]
}

static HEAP: Spinlock<HeapWrapper> = Spinlock::new(HeapWrapper { 
    buffer: [0; TOTAL_BOOT_MEMORY], bitmap: [0; BITMAP_SIZE]
});

static OLD_HEAP_PTR: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub fn get_heap(reg: Regions) -> (*const u8, *const u8) {
    let heap = HEAP.lock();
    let reg_idx = reg as usize; 
    let mut idx = 0;
    let mut heap_offset = 0;
    let mut bitmap_offset = 0;

    while idx < reg_idx {
        heap_offset += BOOT_REGIONS[idx];
        bitmap_offset += (BOOT_REGIONS[idx] / MIN_SLOT_SIZE) >> 3;
        idx += 1;
    }

    let heap_ptr = unsafe {
        heap.buffer.as_ptr().add(heap_offset)
    };

    let bitmap_ptr = unsafe {
        heap.bitmap.as_ptr().add(bitmap_offset)
    };

    (heap_ptr, bitmap_ptr)
}

#[cfg(test)]
pub fn clear_heap() {
    let mut heap = HEAP.lock();
    unsafe {
        heap.bitmap.as_mut_ptr().write_bytes(0, BITMAP_SIZE);
    }
}


// Forces FixedAllocator monomorphization only when slot size (size of the contained data) is >= MIN_SLOT_SIZE
pub struct FixedAllocator<T, const REGION: usize> 
where [(); mem::size_of::<T>() - MIN_SLOT_SIZE]: {
    _marker: PhantomData<T> 
}

impl<T, const REGION: usize> FixedAllocator<T, REGION> 
where [(); mem::size_of::<T>() - MIN_SLOT_SIZE]: {
    // Get the heap and bitmap offset for given allocator region
    const fn fetch_hdr_and_base() -> (usize, usize) {
        let mut heap_offset: usize = 0;
        let mut bitmap_offset: usize = 0;

        let mut idx = 0;
        while idx < REGION {
            heap_offset += BOOT_REGIONS[idx];

            // Here we are segregating the bitmap region conservatively
            // Each bitmap section for a region has space for more slots
            // than would probably be used but this is fine
            bitmap_offset += (BOOT_REGIONS[idx] / MIN_SLOT_SIZE) >> 3;
            idx += 1;
        }

        (heap_offset, bitmap_offset)
    }

    const fn calculate_total_slots() -> usize {
        BOOT_REGIONS[REGION] / mem::size_of::<T>()
    }
}


impl<T, const REGION: usize> Allocator<T>
for FixedAllocator<T, REGION>
where [(); mem::size_of::<T>() - MIN_SLOT_SIZE]: {

    fn alloc(layout: Layout) -> Result<NonNull<T>, KError> {
        assert!(layout.size() != 0 && layout.size() % mem::size_of::<T>() == 0);

        let mut heap = HEAP.lock();
        let (base_offset, hdr_offset) = Self::fetch_hdr_and_base();

        let slot_size = mem::size_of::<T>();
        let num_slots = Self::calculate_total_slots();

        let mut slots_required = ceil_div(layout.size(), slot_size);
        let mut start_slot = 0;
        let mut num_slots_found = 0;

        for slot_idx in 0..num_slots {
            let slot_group_idx = slot_idx >> 3;
            let bit_idx = slot_idx % 8;
            let slot = heap.bitmap[hdr_offset + slot_group_idx] & (1 << bit_idx);

            // Check if we have n contiguous slots 
            if slot == 0 {
                if num_slots_found == 0 {
                    start_slot = slot_idx;
                }
                
                num_slots_found += 1;

                if num_slots_found == slots_required {
                    break;
                }
            }
            else {
                num_slots_found = 0;
            }
        }

        let sel_slot = start_slot;
        if num_slots_found != slots_required {
            info!("Fixed allocator region:{} ran out of space, num_slots:{}, slots_required:{}, num_slots_found:{}!", 
            REGION, num_slots, slots_required, num_slots_found);
            
            return Err(KError::OutOfMemory);
        }

        // Set all those n bits to '1'
        while slots_required > 0 {
            let slot_group_idx = start_slot >> 3;
            let bit_idx = start_slot % 8;
            let slot_group = &mut heap.bitmap[hdr_offset + slot_group_idx];

            *slot_group |= 1 << bit_idx;

            start_slot += 1;
            slots_required -= 1;
        }

        unsafe {
            Ok(NonNull::new(heap.buffer.as_ptr().add(base_offset + sel_slot * slot_size) as *mut T).unwrap())
        }
    }
    
    unsafe fn dealloc(address: NonNull<T>, layout: Layout) {
        assert!(layout.size() != 0 && layout.size() % mem::size_of::<T>() == 0);

        let mut heap = HEAP.lock();
        let (base_offset, hdr_offset) = Self::fetch_hdr_and_base();

        let mut address = address.as_ptr().addr();
        let old_heap_ptr = OLD_HEAP_PTR.load(Ordering::Relaxed);
        let heap_rgn_base = heap.buffer.as_ptr().addr() + base_offset;

        // The allocation was made prior to kernel address space init. Translate those addresses here
        if address >= old_heap_ptr && address < old_heap_ptr + size_of::<HeapWrapper>() {
            address = heap.buffer.as_ptr().addr() + address - old_heap_ptr;
        }

        let total_size = layout.size();
        let slot_size = mem::size_of::<T>();
        let mut slots = ceil_div(total_size, slot_size);
        let mut slot_offset = (address - heap_rgn_base) / slot_size;
        let num_slots = Self::calculate_total_slots(); 

        debug_assert!(slot_offset < num_slots, 
            "Wrong address given to dealloc function for fixed allocator => slot_offset:{}, num_slots:{} for Fixed allocator Region:{}!", 
            slot_offset, num_slots, REGION);

        while slots > 0 {
            let slot_group = slot_offset >> 3;
            let bit_idx = slot_offset % 8;
            
            // Clear that bit in the given byte (0 means free)
            let slot = &mut heap.bitmap[hdr_offset + slot_group];
            *slot = *slot & !(1 << bit_idx);  

            slot_offset += 1;
            slots -= 1;
        }
    }
}

// This function should be called before using fixed allocator routines
pub fn setup_heap() {
    OLD_HEAP_PTR.store(HEAP.lock().buffer.as_ptr().addr() , Ordering::SeqCst);
}