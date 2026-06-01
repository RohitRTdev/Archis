use core::ptr::null_mut;
use core::mem::{size_of, align_of};
use common::{align_up, PAGE_SIZE};
use crate::mem::{allocate_memory, PageDescriptor};
use crate::sync::Spinlock;
use kernel_intf::{info, KError};
use core::alloc::Layout;

pub struct ListNode {
    size: usize,
    next: Option<&'static mut ListNode>,
}

pub struct LinkedListAllocator {
    head: *mut ListNode,
    backing_memory: usize
}

unsafe impl Send for LinkedListAllocator {}

impl LinkedListAllocator {
    pub const fn new() -> Self {
        Self {
            head: core::ptr::null_mut(),
            backing_memory: 0
        }
    }

    fn find_fit(&mut self, size: usize, align: usize) -> Option<*mut ListNode> {
        let mut prev: *mut ListNode = core::ptr::null_mut();
        let mut current = self.head;
        while !current.is_null() {
            let node = unsafe { &mut *current };
            let addr = current as usize;
            let aligned_addr = align_up(addr, align);
            let padding = aligned_addr - addr;
            if node.size >= size + padding {
                // Remove node from list
                if !prev.is_null() {
                    unsafe { (*prev).next = node.next.take(); }
                } else {
                    self.head = node.next.take().map_or(core::ptr::null_mut(), |n| n);
                }
                return Some(current);
            }
            prev = current;
            current = node.next.as_deref_mut().map_or(core::ptr::null_mut(), |n| n as *mut _);
        }
        None
    }

    fn add_free_region(&mut self, addr: usize, size: usize) {
        let node = addr as *mut ListNode;
        unsafe {
            node.write(ListNode {
                size,
                next: self.head.as_mut()
            });
        }
        self.head = node;
    }

    // Given a ListNode pointer, size/align, split the node and return the aligned pointer.
    fn use_list_node(&mut self, node_ptr: *mut ListNode, size: usize, align: usize) -> *mut u8 {
        let node = unsafe { &mut *node_ptr };
        let addr = node_ptr as usize;
        let aligned_addr = align_up(addr, align);
        let next_aligned_addr = align_up(aligned_addr + size, align_of::<ListNode>());

        // Sanity checks to prevent arithmetic underflow/corruption
        debug_assert!(next_aligned_addr >= aligned_addr, "next_aligned_addr must be >= aligned_addr");
        debug_assert!(next_aligned_addr - addr <= node.size, "calculated split exceeds node size");

        let remaining = node.size - (next_aligned_addr - addr);

        // Maintain backing memory accounting
        self.backing_memory = self.backing_memory.saturating_sub(size);

        if remaining >= size_of::<ListNode>() {
            self.add_free_region(next_aligned_addr, remaining);
        }

        aligned_addr as *mut u8
    }
}

static HEAP: Spinlock<LinkedListAllocator> = Spinlock::new(LinkedListAllocator::new());

fn heap_alloc_impl(size: usize, align: usize) -> *mut u8 {
    let size = size.max(size_of::<ListNode>());
    let align = align.max(align_of::<ListNode>());
    let mut allocator = HEAP.lock();

    // If not enough memory is reserved, just skip the search and ask virtual allocator for memory
    if allocator.backing_memory >= size {
        if let Some(node_ptr) = allocator.find_fit(size, align) {
            return allocator.use_list_node(node_ptr, size, align);
        }
    }

    // Out of memory, request more from virtual allocator and retry
    let alloc_size = align_up(size, PAGE_SIZE);
    match allocate_memory(Layout::from_size_align(alloc_size, PAGE_SIZE).unwrap(), PageDescriptor::VIRTUAL).as_ref() {
        Ok(mem) => {
            allocator.add_free_region(*mem as usize, alloc_size);
            allocator.backing_memory += alloc_size;
            if let Some(node_ptr) = allocator.find_fit(size, align) {
                allocator.use_list_node(node_ptr, size, align)
            } else {
                info!("Heap allocator could not find a fit for allocation size:{} and alignment:{} despite adding new memory", size, align);
                null_mut()
            }
        },
        Err(_) => {
            info!("Frame allocator has run out of memory for allocation size:{} and alignment:{}", size, align);
            null_mut()
        }
    }
}

fn heap_dealloc_impl(addr: *mut u8, size: usize, _align: usize) {
    let size = size.max(size_of::<ListNode>());
    let mut allocator = HEAP.lock();
    allocator.add_free_region(addr as usize, size);
    allocator.backing_memory += size;
}

#[unsafe(no_mangle)]
extern "C" fn heap_alloc_ffi(size: usize, align: usize, out: *mut *mut u8) -> KError {
    let ptr = heap_alloc_impl(size, align);
    unsafe { *out = ptr; }
    if ptr.is_null() { KError::OutOfMemory } else { KError::Success }
}

#[unsafe(no_mangle)]
extern "C" fn heap_dealloc_ffi(ptr: *mut u8, size: usize, align: usize) -> KError {
    if ptr.is_null() { return KError::InvalidArgument; }
    heap_dealloc_impl(ptr, size, align);
    KError::Success
}
