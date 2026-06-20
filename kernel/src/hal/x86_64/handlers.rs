use common::{MemoryRegion, PAGE_SIZE};
use kernel_intf::{debug, info};
use core::sync::atomic::{AtomicUsize, Ordering};
use core::ptr::NonNull;
use crate::cpu::{self, MAX_CPUS, PerCpu, general_interrupt_handler};
use crate::hal::x86_64::asm::switch_context_force;
use crate::hal::{enable_scheduler_timer, get_core, get_per_cpu_base, get_per_cpu_kernel_base};
use crate::infra;
use crate::sched::{SIGSEGV, issue_signal};
use crate::sync::Spinlock;
use super::{lapic, timer};
use crate::mem::on_page_fault;
use super::lapic::{eoi, get_error};
use super::cpu::get_bsp_lapic_id;
use super::MAX_INTERRUPT_VECTORS;
use super::asm;
use crate::hal::halt;
use crate::devices::ioapic::add_redirection_entry;
use crate::mem::FixedList;
use kernel_intf::list::List;
use crate::mem::Regions::Region3;

pub const PAGE_FAULT_VECTOR: usize = 14;
pub const DOUBLE_FAULT_VECTOR: usize = 8;
pub const NMI_FAULT_VECTOR: usize = 2;
pub const SPURIOUS_VECTOR: usize = 32;
pub const YIELD_VECTOR: usize = 33;
pub const DEBUG_VECTOR: usize = 34;
pub const TIMER_VECTOR: usize = 35;
pub const ERROR_VECTOR: usize = 36;
pub const IPI_VECTOR: usize = 37;
pub const SYS_VECTOR: usize = 38;
const USER_VECTOR_START: usize = 39;


#[derive(Clone, Copy)]
pub enum IPIRequestType {
    SchedChange,
    TlbInvalidate(MemoryRegion),
    Shutdown
}

struct IPIRequest {
    req_type: IPIRequestType,
    core: usize
}

struct InterruptContext {
    is_interrupt: bool,
    vector: usize
}

static NEXT_AVAILABLE_VECTOR: AtomicUsize = AtomicUsize::new(USER_VECTOR_START);

const EXCEPTION_VECTOR_RANGE: usize = 32;

// This is set at init time and then never changed
pub static mut DEBUG_HANDLER_FN: Option<fn()> = None;

static PER_CPU_GLOBAL_CONTEXT: PerCpu<AtomicUsize> = PerCpu::new_with(
    [const {AtomicUsize::new(0)}; MAX_CPUS]
);

static PER_CPU_NESTED_CONTEXT: PerCpu<AtomicUsize> = PerCpu::new_with(
    [const {AtomicUsize::new(0)}; MAX_CPUS]
);

static IS_INTERRUPT_CONTEXT: PerCpu<Spinlock<InterruptContext>> = PerCpu::new_with(
    [const {Spinlock::new(InterruptContext { is_interrupt: false, vector: 0 })}; MAX_CPUS]
);
static IPI_REQUESTS: Spinlock<FixedList<IPIRequest, {Region3 as usize}>> = Spinlock::new(List::new());

static mut VECTOR_TABLE: [fn(usize); MAX_INTERRUPT_VECTORS] = [default_handler; MAX_INTERRUPT_VECTORS];
const UNDEFINED_STRING: &'static str = "Undefined";
const EXCP_STRINGS: [&'static str; EXCEPTION_VECTOR_RANGE] = [
    "Divide by zero",
    "Debug",
    "NMI",
    "Breakpoint",
    "Overflow",
    "BoundRange",
    "Invalid-opcode",
    "Device-not-available",
    "Double-fault",
    UNDEFINED_STRING,
    "Invalid TSS",
    "Segment-not-present",
    "Stack",
    "General protection",
    "Page fault",
    UNDEFINED_STRING,
    "x87-Floating-point",
    "Alignment-check",
    "Machine-check",
    "SIMD-floating-point",
    UNDEFINED_STRING,
    "Control-protection",
    UNDEFINED_STRING,
    UNDEFINED_STRING,
    UNDEFINED_STRING,
    UNDEFINED_STRING,
    UNDEFINED_STRING,
    UNDEFINED_STRING,
    UNDEFINED_STRING,
    UNDEFINED_STRING,
    UNDEFINED_STRING,
    UNDEFINED_STRING
];

#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct CPUContext {
    pad: u64,
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    vector: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64
}

// We require stack to be 16 byte aligned
const _: () = {
    assert!(core::mem::size_of::<CPUContext>() % 16 == 0);
};

impl CPUContext {
    const fn new() -> Self {
        CPUContext { pad: 0, r15: 0, r14: 0, r13: 0, r12: 0, r11: 0, r10: 0, r9: 0, r8: 0, rbp: 0, rdi: 0, rsi: 0, 
            rdx: 0, rcx: 0, rbx: 0, rax: 0, vector: 0, rip: 0, cs: 0, rflags: 0, rsp: 0, ss: 0 
        }
    }
}

fn set_interrupt_context(is_interrupt: bool, vector: usize) {
    let mut int_context = IS_INTERRUPT_CONTEXT.local().lock();
    int_context.is_interrupt = is_interrupt;
    int_context.vector = vector;
}

pub fn force_context_switch() {
    {
        let mut int_context = IS_INTERRUPT_CONTEXT.local().lock();
        assert!(int_context.is_interrupt);
        
        if int_context.vector as usize > DEBUG_VECTOR && int_context.vector as usize != SYS_VECTOR {
            eoi();
        }
        
        int_context.is_interrupt = false;
        int_context.vector = 0;
    }
    let con = PER_CPU_GLOBAL_CONTEXT.local().load(Ordering::Acquire) as *const CPUContext;

    unsafe { switch_context_force(con.addr() as u64); }
}

#[unsafe(no_mangle)]
extern "C" fn global_interrupt_handler(vector: u64, cpu_context: *const CPUContext) -> *const CPUContext {
    let in_dw = crate::sched::is_in_dw_mode();
    set_interrupt_context(true, vector as usize);

    // While in DW mode the original interrupted-task context still lives in
    // PER_CPU_GLOBAL_CONTEXT and must not be touched 
    let slot = if in_dw { &PER_CPU_NESTED_CONTEXT } else { &PER_CPU_GLOBAL_CONTEXT };
    slot.local().store(cpu_context.addr(), Ordering::Release);

    unsafe {
        VECTOR_TABLE[vector as usize](vector as usize);
    }

    if vector as usize > DEBUG_VECTOR && vector as usize != SYS_VECTOR {
        eoi();
    }

    // Only drain the DW queue when this interrupt did not itself happen during a DW.
    if !in_dw {
        crate::sched::dw_handler();
    }
    
    set_interrupt_context(false, 0);
    slot.local().load(Ordering::Acquire) as *const CPUContext
}

fn default_handler(idx: usize) {
    panic!("Called default handler on vector: {}, {:?}", idx, unsafe{*(fetch_context() as *const CPUContext)});
}

pub fn init() {
    unsafe {
        for vector in 0..EXCEPTION_VECTOR_RANGE {
            VECTOR_TABLE[vector] = |idx| {
                // In these cases, we switch to different stack
                // Even though it's possible to still print the callstack, we don't do it for now
                if idx == NMI_FAULT_VECTOR || idx == DOUBLE_FAULT_VECTOR {
                    infra::disable_callstack();
                }
                let con = *(fetch_context() as *const CPUContext);
                debug!("{:?}", con);
                debug!("gs={:#X}, kernel_gs={:#X}", get_per_cpu_kernel_base(), get_per_cpu_base());

                if idx == DOUBLE_FAULT_VECTOR {
                    panic!("{} exception!\nPossible stack overflow??", EXCP_STRINGS[idx]);
                }

                // NMI is a hardware event (memory error, watchdog) — unrelated to the
                // faulting process, so always panic regardless of privilege level.
                if idx == NMI_FAULT_VECTOR {
                    panic!("{} exception!", EXCP_STRINGS[idx]);
                }

                // For all other exceptions originating in user space, kill the process
                // instead of crashing the kernel.
                try_kill_user_process(EXCP_STRINGS[idx]);

                panic!("{} exception!", EXCP_STRINGS[idx]);
            };
        }

        VECTOR_TABLE[PAGE_FAULT_VECTOR] = page_fault_handler;

        for vector in USER_VECTOR_START..MAX_INTERRUPT_VECTORS {
            VECTOR_TABLE[vector] = general_interrupt_handler;
        }

        VECTOR_TABLE[SPURIOUS_VECTOR] = spurious_handler;
        VECTOR_TABLE[DEBUG_VECTOR] = debug_handler;
        VECTOR_TABLE[YIELD_VECTOR] = yield_handler;
        VECTOR_TABLE[TIMER_VECTOR] = timer_handler;
        VECTOR_TABLE[ERROR_VECTOR] = error_handler;
        VECTOR_TABLE[IPI_VECTOR] = ipi_handler;
        VECTOR_TABLE[SYS_VECTOR] = sys_handler;
    }
    info!("Initialized interrupt handlers");
}

// Interrupts must be disabled during this call
pub fn register_interrupt_handler(irq: usize, active_high: bool, is_edge_triggered: bool) -> usize {
    let vector = NEXT_AVAILABLE_VECTOR.fetch_add(1, Ordering::Relaxed);

    // We will tie up all IOAPIC interrupts to BSP
    add_redirection_entry(irq, get_bsp_lapic_id(), vector, active_high, is_edge_triggered);    
    
    vector
}

fn spurious_handler(_vector: usize) {
    debug!("Detected spurious interrupt!");
}

fn debug_handler(_vector: usize) {
    info!("Calling debug handler layer");
    unsafe {
        if let Some(handler) = DEBUG_HANDLER_FN {
            handler();
        }
    }
}

// It's fine to handle these without locks since CPU won't interrupt during this call
// This is true since we are already in interrupt handler and further interrupts are masked by current design
fn timer_handler(_vector: usize) {
    crate::sched::schedule();

    // Reload the timer
    lapic::setup_timer_value(timer::BASE_COUNT.local().load(Ordering::Acquire) as u32);
}

// Do the same thing as timer handler, except we don't reload the timer register and we won't send EOI
fn yield_handler(_vector: usize) {
    crate::sched::schedule();
}

fn error_handler(_vector: usize) {
    info!("Error status register: {:#X}", get_error() & 0xff);
}

fn sys_handler(_vector: usize) {
    unsafe {
        info!("Sys handler context: {:?}", *(fetch_context() as *const CPUContext));
    }
}

fn page_fault_handler(_vector: usize) {
    let fault_address = asm::read_cr2();
    
    debug!("{:?}", unsafe {*(fetch_context() as *const CPUContext)});

    on_page_fault(fault_address as usize);
}

pub fn is_system_in_interrupt_context() -> bool {
    IS_INTERRUPT_CONTEXT.local().lock().is_interrupt
}

pub fn fetch_context() -> usize {
    assert!(!crate::sched::is_in_dw_mode(), "fetch_context() called while in DW mode");
    PER_CPU_GLOBAL_CONTEXT.local().load(Ordering::Acquire)
}

pub fn get_user_stack(context: usize) -> usize {
    let con = unsafe { &*(context as *const CPUContext) };
    con.rsp as usize
}

pub fn is_user_context(context: usize) -> bool {
    let con = unsafe { &*(context as *const CPUContext) };
    con.cs & 3 == 3
}

// If the current fault context is user space and a user process is active,
// kill that process with exit code -1 
pub fn try_kill_user_process(fault_label: &str) {
    if is_user_context(fetch_context()) {
        if let Some(pid) = crate::sched::get_current_process_id() {
            info!("{} in user process — killing process", fault_label);
            issue_signal(pid, SIGSEGV);
            crate::sched::yield_cpu();
        }
    }
}

pub fn switch_context(new_context: usize) {
    assert!(!crate::sched::is_in_dw_mode(), "switch_context() called while in DW mode");
    PER_CPU_GLOBAL_CONTEXT.local().store(new_context, Ordering::Release);
}

pub fn create_context_from(handler: extern "C" fn() -> !, stack_base: *mut u8, actual_base: usize, context: usize) -> usize {
    let mut sp = stack_base as usize;
    assert!(sp & 0xF == 0, "create_context_from() -> unaligned stack pointer!");
    assert!(actual_base & 0xF == 0, "create_context_from() -> unaligned stack pointer!");

    sp -= core::mem::size_of::<CPUContext>();
    let new_context = sp as *mut CPUContext;
    let context = context as *const CPUContext;

    unsafe {
        core::ptr::copy_nonoverlapping(context as *const u8, new_context as *mut u8, size_of::<CPUContext>());
        let new_context = &mut *new_context;
        new_context.rip = handler as u64;
        new_context.rsp = actual_base as u64;
        new_context.rbp = 0;
    } 

    sp
}

pub fn create_user_context(handler: extern "C" fn() -> !, stack_base: *mut u8, actual_base: usize) -> usize {
    let mut sp = stack_base as usize;
    assert!(sp & 0xF == 0, "create_context() -> unaligned stack pointer!");
    assert!(actual_base & 0xF == 0, "create_context_from() -> unaligned stack pointer!");

    // 16 byte alignment is maintained since stack_base already aligned to 4096 bytes
    sp -= core::mem::size_of::<CPUContext>();

    let mut context = CPUContext::new();
    context.rip = handler as u64;
    context.rbp = 0; // Stops backtrace walkers at the base frame
    
    context.rsp = actual_base as u64;

    // user code + user data
    context.cs = 0x23;
    context.ss = 0x1b;
    context.rflags = unsafe {
        super::cpu_regs::INIT_RFLAGS
    };


    unsafe {
        (sp as *mut CPUContext).write(context);
    };

    sp    
}

pub fn create_kernel_context(handler: extern "C" fn() -> !, stack_base: *mut u8) -> usize {
    let mut sp = stack_base as usize;
    assert!(sp & 0xF == 0, "create_context() -> unaligned stack pointer!");

    // 16 byte alignment is maintained since stack_base already aligned to 4096 bytes
    sp -= core::mem::size_of::<CPUContext>();

    let mut context = CPUContext::new();
    context.rip = handler as u64;
    context.rbp = 0; // Stops backtrace walkers at the base frame
    
    // SysV x86_64 ABI: at function entry RSP must be 16-byte aligned minus 8
    context.rsp = (stack_base.addr() - 8) as u64;

    // Kernel code + Kernel data
    context.cs = 0x8;
    context.ss = 0x10;
    context.rflags = unsafe {
        super::cpu_regs::INIT_RFLAGS
    };


    unsafe {
        (sp as *mut CPUContext).write(context);
    };

    sp
}

fn ipi_handler(_vector: usize) {
    let core = get_core();
    loop {
        let mut req = {
            let mut ipi_queue = IPI_REQUESTS.lock();
            let mut found_ptr = None;

            for node in ipi_queue.iter() {
                if node.core == core {
                    found_ptr = Some(NonNull::from(node));
                    break;
                }
            }

            found_ptr.map(|ptr| unsafe { ipi_queue.remove_node(ptr) })
        };

        match &mut req {
            Some(req_info) => {
                match req_info.req_type {
                    IPIRequestType::SchedChange => {
                        enable_scheduler_timer();
                    },
                    IPIRequestType::TlbInvalidate(desc) => {
                        // Reload cr3
                        unsafe {
                            for page in 0..desc.size / PAGE_SIZE {
                                let real_page = (page * PAGE_SIZE + desc.base_address) as u64;
                                asm::invlpg(real_page);
                            }
                        }
                    },
                    IPIRequestType::Shutdown => {
                        halt();
                    }
                }
            },
            None => {
                break;
            }
        }
    }
}

// Function should only be called after scheduler is up
// if wait_for_completion is set, caller needs to ensure that no locks are held during a call to notify_core
// Otherwise, this may lead to deadlock
pub fn notify_core(req_type: IPIRequestType, target_core: usize) {
    assert!(target_core < cpu::get_total_cores());
    
    let apic_id = super::get_apic_id(target_core);

    let req = IPIRequest {
        req_type, 
        core: target_core
    };

    let mut req_queue = IPI_REQUESTS.lock();
    let res = req_queue.add_node(req);
    if res.is_err() {
        // Infra uses this, so drop the lock
        drop(req_queue);
        panic!("Failed to queue ipi request");
    }

    lapic::send_ipi(apic_id as u32, IPI_VECTOR as u8);
}