use alloc::collections::BTreeMap;
use crate::Spinlock;
use crate::hal::{disable_interrupts, enable_interrupts, register_interrupt_handler};
use kernel_intf::debug;

// TODO: Have ability to chain interrupts
#[allow(dead_code)]
struct InterruptDescriptor {
    irq: usize,
    handler: fn(usize)
}

struct InterruptHandlerBlock {
    handlers: BTreeMap<usize, InterruptDescriptor>
}

static INTERRUPT_HANDLERS: Spinlock<InterruptHandlerBlock> = Spinlock::new(InterruptHandlerBlock{handlers: BTreeMap::new()});

pub fn general_interrupt_handler(vector: usize) {
    if let Some(desc) = INTERRUPT_HANDLERS.lock().handlers.get(&vector) {
        (desc.handler)(vector);
    }      
    else {
        debug!("Spurious interrupt detected at vector: {}", vector);
    }
}

pub fn install_interrupt_handler(irq: usize, handler: fn(usize), active_high: bool, is_edge_triggered: bool) {
    let int_stat = disable_interrupts();
    let vector = register_interrupt_handler(irq, active_high, is_edge_triggered);

    INTERRUPT_HANDLERS.lock().handlers.insert(vector, InterruptDescriptor {irq, handler});
    enable_interrupts(int_stat);
}

#[unsafe(no_mangle)]
pub extern "C" fn install_interrupt_handler_ffi(
    irq: usize,
    handler: extern "C" fn(usize),
    active_high: bool,
    is_edge_triggered: bool,
) {
    // extern "C" fn(usize) and fn(usize) are ABI-compatible at ring 0 on x86_64.
    install_interrupt_handler(irq, unsafe { core::mem::transmute(handler) }, active_high, is_edge_triggered);
}