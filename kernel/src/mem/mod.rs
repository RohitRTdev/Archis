mod fixed_allocator;
mod frame_allocator;
mod virtual_allocator;
mod heap_allocator;
mod pool_allocator;
pub use fixed_allocator::*;
pub use frame_allocator::*;
pub use virtual_allocator::*;

// This is in canonical form
#[cfg(target_arch="x86_64")]
pub const KERNEL_HALF_OFFSET: usize = 0xffff800000000000;
const KERNEL_HALF_OFFSET_RAW: usize = 0x0000800000000000;

// Canonical definition lives in kernel_intf so drivers (which allocate/map
// MMIO and DMA memory through the kernel_intf FFI) share the exact same
// flags the kernel's own allocator uses internally.
pub use kernel_intf::mem::PageDescriptor;

pub fn init() {
    frame_allocator_init();
    virtual_allocator_init();
}
