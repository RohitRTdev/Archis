use alloc::collections::BTreeMap;
use core::ptr::NonNull;

use kernel_intf::list::{DynList, List, ListNode};
use kernel_intf::{InterruptHandle, InterruptRoutine, debug};
use crate::Spinlock;
use crate::hal::{disable_interrupts, enable_interrupts, register_interrupt_handler};

struct InterruptDescriptor {
    context: *mut core::ffi::c_void,
    handler: InterruptRoutine,
}

unsafe impl Send for InterruptDescriptor {}

struct InterruptHandlerBlock {
    irq_mapping:    BTreeMap<usize, (usize, DynList<InterruptDescriptor>)>,
    vector_mapping: BTreeMap<usize, usize>,
}

static INTERRUPT_HANDLERS: Spinlock<InterruptHandlerBlock> = Spinlock::new(
    InterruptHandlerBlock {
        irq_mapping:    BTreeMap::new(),
        vector_mapping: BTreeMap::new(),
    }
);

pub fn general_interrupt_handler(vector: usize) {
    let int_handlers = INTERRUPT_HANDLERS.lock();
    if let Some(irq) = int_handlers.vector_mapping.get(&vector) {
        // Any device on this IRQ could have raised the interrupt.
        // Walk the chain until one handler claims it (returns true).
        for desc in int_handlers.irq_mapping.get(irq).unwrap().1.iter() {
            if (desc.handler)(desc.context) {
                break;
            }
        }
    } else {
        debug!("Spurious interrupt detected at vector: {}", vector);
    }
}

pub fn install_interrupt_handler(
    irq: usize,
    context: *mut core::ffi::c_void,
    handler: InterruptRoutine,
    active_high: bool,
    is_edge_triggered: bool,
) -> InterruptHandle {
    let int_stat = disable_interrupts();

    let descriptor = InterruptDescriptor { handler, context };

    let node: NonNull<ListNode<InterruptDescriptor>> = {
        let mut desc = INTERRUPT_HANDLERS.lock();

        if desc.irq_mapping.contains_key(&irq) {
            // Chain onto the existing handler list for this IRQ.
            desc.irq_mapping
                .get_mut(&irq)
                .unwrap()
                .1
                .add_node_get_handle(descriptor)
        } else {
            // First handler for this IRQ — allocate a vector.
            let vector = register_interrupt_handler(irq, active_high, is_edge_triggered);
            desc.irq_mapping.insert(irq, (vector, List::new()));
            let r = desc.irq_mapping
                .get_mut(&irq)
                .unwrap()
                .1
                .add_node_get_handle(descriptor);
            desc.vector_mapping.insert(vector, irq);
            r
        }
    }.expect("OOM: interrupt handler install");

    enable_interrupts(int_stat);

    InterruptHandle {
        irq,
        node_ptr: node.as_ptr() as usize,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn io_install_interrupt_handler_ffi(
    irq: usize,
    context: *mut core::ffi::c_void,
    handler: InterruptRoutine,
    active_high: bool,
    is_edge_triggered: bool,
) -> InterruptHandle {
    install_interrupt_handler(irq, context, handler, active_high, is_edge_triggered)
}

#[unsafe(no_mangle)]
pub extern "C" fn io_remove_interrupt_handler_ffi(handle: InterruptHandle) {
    let int_stat = disable_interrupts();

    {
        let mut desc = INTERRUPT_HANDLERS.lock();

        let mut remove_vector = None;
        if let Some((vector, list)) = desc.irq_mapping.get_mut(&handle.irq) {
            let node = unsafe {
                NonNull::new_unchecked(handle.node_ptr as *mut ListNode<InterruptDescriptor>)
            };
            unsafe { list.remove_node(node); }

            if list.get_nodes() == 0 {
                remove_vector = Some(*vector);
            }
        }

        if let Some(vector) = remove_vector {
            desc.irq_mapping.remove(&handle.irq);
            desc.vector_mapping.remove(&vector);
        }
    }

    enable_interrupts(int_stat);
}
