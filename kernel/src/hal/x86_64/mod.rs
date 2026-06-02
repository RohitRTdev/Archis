use kernel_intf::info;
use crate::mem::MapFetchType;
use core::sync::atomic::{AtomicU64, Ordering};
mod asm;
mod utils;
mod features;
mod cpu_regs;
mod page_mapper;
mod tables;
mod handlers;
mod cpu;
mod timer;
mod lapic;
mod syscall;

#[cfg(not(test))]
mod smp;

pub use cpu::*;
pub use utils::*;
pub use page_mapper::*;
pub use handlers::*;
pub use tables::*;
pub use syscall::*;

const MAX_INTERRUPT_VECTORS: usize = 256;


#[cfg(not(test))]
pub fn disable_interrupts() -> bool {
    let flags: u64;
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {}",
            "cli",
            out(reg) flags
        );
    }

    // RFLAGS register bit 9 is IF -> 1 is enabled
    let is_int_enabled = (flags & (1 << 9)) != 0;
    
    is_int_enabled
}

#[cfg(not(test))]
pub fn enable_interrupts(int_status: bool) {
    if int_status {
        unsafe { 
            core::arch::asm!(
                "sti"
            )
        }
    }
}

#[cfg(not(test))]
pub fn are_interrupts_enabled() -> bool {
    // RFLAGS bit 9 is IF -> 1 means interrupts enabled.
    (asm::read_rflags() & (1 << 9)) != 0
}

#[cfg(test)]
pub fn disable_interrupts() -> bool {
    true
}

#[cfg(test)]
pub fn enable_interrupts(_: bool) {
}

#[cfg(test)]
pub fn are_interrupts_enabled() -> bool {
    true
}

pub use asm::read_port_u8;
pub use asm::write_port_u8;

pub struct Spinlock {
    state: AtomicU64
}

impl Spinlock {
    pub const fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn lock(&self) {
        while self.state.compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
    }

    #[inline]
    pub fn try_lock(&self) -> bool {
        self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    #[inline]
    pub fn unlock(&self) {
        self.state.store(0, Ordering::Release);
    }
}

#[cfg(not(test))]
#[inline(always)]
pub fn halt() -> ! {
    loop {
        unsafe {
            core::arch::asm!(
                "cli",
                "hlt",
                options(nostack)
            );
        }
    }
}

#[cfg(not(test))]
#[inline(always)]
pub fn sleep() -> ! {
    loop {
        unsafe {
            core::arch::asm!(
                "sti",
                "hlt",
                options(nostack)
            );
        }
    }
}

#[cfg(not(test))]
#[inline(always)]
pub fn yield_cpu() {
    unsafe {
        core::arch::asm!("int 33");
    }
}

#[cfg(debug_assertions)]
#[inline(always)]
pub fn get_current_stack_base() -> usize {
    let rbp;
    unsafe {
        core::arch::asm!("mov {}, rbp",
            out(reg) rbp,
            options(nomem, preserves_flags, nostack));
    }

    rbp
}

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn get_current_stack_base() -> usize {
    // Cannot rely on rbp for optimized build, since compiler may not even use it for tracking frames
    let rsp;
    unsafe {
        core::arch::asm!("mov {}, rsp",
            out(reg) rsp,
            options(nomem, preserves_flags, nostack));
    }

    rsp
}


#[allow(dead_code)]
#[inline(always)]
pub fn fire_debug_interrupt() {
    unsafe {
        core::arch::asm!("int 34");
    }
}

#[cfg(debug_assertions)]
pub fn unwind_stack(max_depth: usize, stack_base: usize, address: &mut [usize]) -> (usize, usize) {
    let init_base = get_current_stack_base();
    let mut base = init_base;
    let mut depth=  0;
    while depth < max_depth && stack_base >= base + 8 {
        let prev_base = base;
        let fn_addr = unsafe {*((base + 8) as *const u64)} as usize;
        base = unsafe {*(base as *const u64)} as usize;
        
        if base <= prev_base {
            break;
        }

        address[depth] = fn_addr;
        depth += 1;
    }

    (depth, init_base)
}

pub fn init() -> ! {
    info!("Starting platform initialization");

    features::init();
    cpu_regs::init();
    
    crate::mem::init();

    let stack_base = crate::cpu::get_current_stack_base();

    switch_to_new_address_space(page_mapper::get_kernel_pml4(), stack_base,
        crate::mem::get_virtual_address(tables::kern_addr_space_start as *const () as usize, 0,  MapFetchType::Kernel).expect("kern_addr_space_start virtual address not found!"));
}

#[allow(dead_code)]
pub fn register_debug_fn(handler: fn()) {
    unsafe {
        handlers::DEBUG_HANDLER_FN = Some(handler);
    }
}

pub fn enable_scheduler_timer() {
    lapic::enable_timer(timer::BASE_COUNT.local().load(Ordering::Acquire) as u32);
}

pub fn disable_scheduler_timer() {
    lapic::disable_timer();
}


