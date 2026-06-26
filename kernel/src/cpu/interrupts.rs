use alloc::collections::BTreeMap;
use core::ptr::NonNull;

use kernel_intf::list::{DynList, List, ListNode};
use kernel_intf::{InterruptHandle, InterruptRoutine, debug};
use crate::Spinlock;
use crate::hal::{disable_interrupts, enable_interrupts, register_interrupt_handler, unregister_interrupt_handler};

struct InterruptDescriptor {
    context: *mut core::ffi::c_void,
    handler: InterruptRoutine,
}

unsafe impl Send for InterruptDescriptor {}

struct InterruptHandlerBlock {
    irq_mapping:    BTreeMap<usize, (usize, DynList<InterruptDescriptor>)>,
    vector_to_irq_mapping: BTreeMap<usize, usize>,
    vector_mapping: BTreeMap<usize, InterruptDescriptor> 
}

static INTERRUPT_HANDLERS: Spinlock<InterruptHandlerBlock> = Spinlock::new(
    InterruptHandlerBlock {
        irq_mapping:    BTreeMap::new(),
        vector_to_irq_mapping: BTreeMap::new(),
        vector_mapping: BTreeMap::new()
    }
);

pub fn general_interrupt_handler(vector: usize) {
    let int_handlers = INTERRUPT_HANDLERS.lock();
    // First check the regular vector_mapping
    if let Some(desc) = int_handlers.vector_mapping.get(&vector) {
        (desc.handler)(desc.context);
    }
    else {
        if let Some(irq) = int_handlers.vector_to_irq_mapping.get(&vector) {
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
}

pub fn install_interrupt_handler(
    vector: usize,
    irq: usize,
    context: *mut core::ffi::c_void,
    handler: InterruptRoutine,
    active_high: bool,
    is_edge_triggered: bool,
) -> InterruptHandle {
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
            register_interrupt_handler(vector, irq, active_high, is_edge_triggered);
            desc.irq_mapping.insert(irq, (vector, List::new()));
            let r = desc.irq_mapping
                .get_mut(&irq)
                .unwrap()
                .1
                .add_node_get_handle(descriptor);
            desc.vector_to_irq_mapping.insert(vector, irq);
            r
        }
    }.expect("OOM: interrupt handler install");

    InterruptHandle {
        irq: irq as isize,
        vector,
        node_ptr: node.as_ptr() as usize,
    }
}

// if irq = -1, we only install the handler, irq registration is not done at hardware level
// This is to help register non-irq based interrupt methods
#[unsafe(no_mangle)]
pub extern "C" fn io_install_interrupt_handler_ffi(
    vector: usize,
    irq: isize,
    context: *mut core::ffi::c_void,
    handler: InterruptRoutine,
    active_high: bool,
    is_edge_triggered: bool
) -> InterruptHandle {
    let int_stat = disable_interrupts();
    let handle = if irq == -1 {
        let mut desc = INTERRUPT_HANDLERS.lock();
        let int_desc = InterruptDescriptor {context, handler};

        // Must not already be allocated as part of an irq
        assert!(desc.vector_to_irq_mapping.get(&vector).is_none());
        assert!(desc.vector_mapping.insert(vector, int_desc).is_none());
        InterruptHandle { irq, vector, node_ptr: 0}
    }
    else {
        install_interrupt_handler(vector, irq as usize, context, handler, active_high, is_edge_triggered)
    };
    
    enable_interrupts(int_stat);

    handle
}

#[unsafe(no_mangle)]
pub extern "C" fn io_remove_interrupt_handler_ffi(handle: InterruptHandle) {
    let int_stat = disable_interrupts();

    {
        let mut desc = INTERRUPT_HANDLERS.lock();
        if handle.irq == -1 {
            assert!(desc.vector_mapping.remove(&handle.vector).is_some());
        }
        else {
            let irq = handle.irq as usize;
            let mut remove_vector = None;
            if let Some((vector, list)) = desc.irq_mapping.get_mut(&irq) {
                let node = unsafe {
                    NonNull::new_unchecked(handle.node_ptr as *mut ListNode<InterruptDescriptor>)
                };
                unsafe { list.remove_node(node); }

                if list.get_nodes() == 0 {
                    remove_vector = Some(*vector);
                }
            }

            // The last handler that belongs to this irq has been removed
            // We can tell hardware to not forward requests from this irq
            if let Some(vector) = remove_vector {
                desc.irq_mapping.remove(&irq);
                desc.vector_to_irq_mapping.remove(&vector);
                unregister_interrupt_handler(irq, vector);
            }
        }
    }

    enable_interrupts(int_stat);
}
