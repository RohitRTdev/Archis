use core::alloc::Layout;
use core::ptr::NonNull;

use alloc::vec::Vec;
use kernel_intf::list::{DynList, List, ListNodeGuard};
use kernel_intf::mem::PoolAllocator;
use common::{MemoryRegion, PAGE_SIZE};

use crate::mem::{VCB, VirtMemConBlk, deallocate_memory};
use crate::sync::{KEvent, Once, Spinlock};
use crate::sched::{self, DispatchRoutine};
use super::proc::HandleType;

pub struct ProcessCleanupWork {
    proc_id:     usize, 
    addr_space:  VCB,
    memory_list: DynList<MemoryRegion>,
    handles:     Vec<Option<HandleType>>
}

impl Default for ProcessCleanupWork {
    fn default() -> Self {
        ProcessCleanupWork {
            proc_id: 0, 
            addr_space: NonNull::dangling(), 
            memory_list: List::new(), 
            handles: Vec::new() 
        }
    }
}

impl ProcessCleanupWork {
    pub fn new(
        proc_id: usize,
        addr_space: VCB,
        memory_list: DynList<MemoryRegion>,
        handles: Vec<Option<HandleType>>
    ) -> Self {
        Self {
            proc_id,
            addr_space,
            memory_list,
            handles
        }
    }
}

unsafe impl Send for ProcessCleanupWork {}
unsafe impl Sync for ProcessCleanupWork {}

static CLEANUP_QUEUE: Spinlock<DynList<ProcessCleanupWork>> = Spinlock::new(List::new());
static CLEANUP_EVENT: Once<KEvent> = Once::new();

pub fn enqueue_cleanup(work: ProcessCleanupWork) {
    CLEANUP_QUEUE.lock().add_node(work).expect("cleanup queue: enqueue failed");
    CLEANUP_EVENT.get().expect("cleanup::init() not called before enqueue_cleanup").signal();
}

fn pop_one() -> Option<ListNodeGuard<ProcessCleanupWork, PoolAllocator>> {
    let mut q = CLEANUP_QUEUE.lock();
    if q.get_nodes() == 0 {
        return None;
    }
    let head = NonNull::from(q.first().unwrap());
    Some(unsafe { q.remove_node(head) })
}

extern "C" fn cleanup_worker() -> ! {
    kernel_intf::info!("Started cleanup worker thread");
    loop {
        CLEANUP_EVENT.get().unwrap().wait(false);
        loop {
            let mut guard = match pop_one() {
                Some(g) => g,
                None => break,
            };

            crate::sched_log!("Destroying resources for process {}", guard.proc_id);

            // Take handles out first so we control when Close IRPs fire.
            let handles = core::mem::take(&mut guard.handles);
            drop(handles);

            unsafe { VirtMemConBlk::destroy_address_space(guard.addr_space); }

            // Deallocate every physical memory region.
            for range in guard.memory_list.iter() {
                crate::sched_log!(
                    "cleanup worker: deallocating base={:#X} size={}",
                    range.base_address, range.size
                );
                deallocate_memory(
                    range.base_address as *mut u8,
                    Layout::from_size_align(range.size, PAGE_SIZE).unwrap(),
                    0,
                ).expect("cleanup worker: dealloc failed");
            }
        }
    }
}

pub fn init() {
    CLEANUP_EVENT.call_once(|| KEvent::new(false));
    sched::create_system_thread(
        cleanup_worker as DispatchRoutine,
        core::ptr::null_mut(),
    ).expect("Failed to create process cleanup worker");
}
