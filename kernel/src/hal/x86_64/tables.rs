use common::{en_flag, ptr_to_usize};
use crate::{cpu::{self, MAX_CPUS, PerCpu}};
use crate::hal::enable_interrupts;
use crate::sync::Spinlock;
use kernel_intf::{debug, info};
use super::{asm, syscall, MAX_INTERRUPT_VECTORS, handlers, lapic, timer, init_per_cpu_data};

#[cfg(not(test))]
use super::smp;

const KERNEL_CODE_SELECTOR: usize = 0x8;

struct GDT;
struct TSSDescriptor;
struct IDTDescriptor;

impl GDT {
    const L: u64 = 1 << 53;
    const P: u64 = 1 << 47;
    const DPL_SHIFT: u64 = 45;
    const CODE: u64 = 0x1A << 40;
    const DATA: u64 = 0x12 << 40;

    const fn new(code_segment: bool, long_mode: bool, present: bool, privilege: u64) -> u64 {
        en_flag!(long_mode, Self::L) | en_flag!(present, Self::P) | (privilege << Self::DPL_SHIFT) | 
        if code_segment {
            Self::CODE
        } else {
            Self::DATA
        }
    }
}

impl TSSDescriptor {
    const TSS_TYPE: u64 = 0x9;
    const TYPE_SHIFT: u64 = 40;
    const P: u64 = 1 << 47; 
    const SEG_UPPER_SHIFT: u64 = 48 - 16;
    const SEG_UPPER_MASK: u64 = 0xF << 16;
    const SEG_LOWER_MASK: u64 = 0xFFFF;
    const ADDRESS_LOWER_MASK: u64 = 0xFFFFFF;
    const ADDRESS_LOWER_SHIFT: u64 = 16;
    const ADDRESS_UPPER_MASK: u64 = 0xFFFFFFFF << 32;
    const ADDRESS_UPPER_SHIFT: u64 = 32;
    const ADDRESS_MIDDLE_MASK: u64 = 0xFF << 24;
    const ADDRESS_MIDDLE_SHIFT: u64 = 56 - 24;

    fn new(seg_limit: u64, base_address: u64) -> [u64; 2] {
        [(Self::TSS_TYPE << Self::TYPE_SHIFT) | Self::P | (seg_limit & Self::SEG_LOWER_MASK) | ((seg_limit & Self::SEG_UPPER_MASK) << Self::SEG_UPPER_SHIFT)
        | ((base_address & Self::ADDRESS_LOWER_MASK) << Self::ADDRESS_LOWER_SHIFT) | ((base_address & Self::ADDRESS_MIDDLE_MASK) << Self::ADDRESS_MIDDLE_SHIFT), 
        (base_address & Self::ADDRESS_UPPER_MASK) >> Self::ADDRESS_UPPER_SHIFT] 
    }
}

#[repr(C, packed)]
pub struct TaskStateSegment {
    _reserved1: u32,
    rsp0: u64,       
    rsp1: u64,       
    rsp2: u64,       
    _reserved2: u64,
    ist: [u64; 7],   
    _reserved3: u64,
    _reserved4: u16,
    iomap_base: u16
}

impl TaskStateSegment {
    pub const fn new(stack_address: u64, good_stack: u64) -> Self {
        let mut task = Self {
            _reserved1: 0,
            rsp0: stack_address,
            rsp1: 0,
            rsp2: 0,
            _reserved2: 0,
            ist: [0; 7],
            _reserved3: 0,
            _reserved4: 0,
            // This along with the limit in TSSDescriptor effectively disables IOPB permission bitmap
            iomap_base: core::mem::size_of::<Self>() as u16,
        };

        task.ist[0] = good_stack;
        task
    }

    pub const fn create() -> Self {
        Self {
            _reserved1: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            _reserved2: 0,
            ist: [0; 7],
            _reserved3: 0,
            _reserved4: 0,
            iomap_base: core::mem::size_of::<Self>() as u16,
        }
    }
}

impl IDTDescriptor {
    const TARGET_ADDR_LOW_MASK: u64 = 0xFFFF;
    const TARGET_ADDR_HIGH_MASK: u64 = 0xFFFFFFFF << 32;
    const TARGET_ADDR_HIGH_SHIFT: u64 = 32;
    const TARGET_ADDR_MIDDLE_MASK: u64 = 0xFFFF << 16;
    const TARGET_ADDR_MIDDLE_SHIFT: u64 = 48 - 16;
    const P: u64 = 1 << 47;
    const DPL: u64 = 0x3 << 45;
    const TYPE_IDT: u64 = 0xE;
    const TYPE_SHIFT: u64 = 40;
    const SELECTOR_SHIFT: u64 = 16;
    const IST0_SHIFT: u64 = 32;

    fn new(selector: u64, handler_address: u64, set_ist: bool) -> [u64; 2] {
        [Self::P | Self::DPL | (Self::TYPE_IDT << Self::TYPE_SHIFT) | ((if set_ist {1} else {0}) << Self::IST0_SHIFT) |
        (selector << Self::SELECTOR_SHIFT) | (handler_address & Self::TARGET_ADDR_LOW_MASK) |
        ((handler_address & Self::TARGET_ADDR_MIDDLE_MASK) << Self::TARGET_ADDR_MIDDLE_SHIFT),
        (handler_address & Self::TARGET_ADDR_HIGH_MASK) >> Self::TARGET_ADDR_HIGH_SHIFT]
    }
}

#[repr(C, packed)]
#[derive(Debug)]
struct TableLayout {
    limit: u16,
    base_address: u64   
}

#[repr(align(8))]
struct TableData {
    gdt_array: [u64; 7],
    gdt_layout: TableLayout
}

#[repr(align(8))]
struct IdtData {
    idt: [u64; MAX_INTERRUPT_VECTORS * 2],
    idt_layout: TableLayout
}

static CPU_TABLE_DATA: PerCpu<Spinlock<TableData>> = PerCpu::new_with(
   [const {Spinlock::new (TableData { gdt_array: [0; 7],
    gdt_layout: TableLayout { limit: 0, base_address: 0 }})}; MAX_CPUS]);

static IDT_DATA: Spinlock<IdtData> = Spinlock::new (IdtData {idt: [0; MAX_INTERRUPT_VECTORS * 2],
idt_layout: TableLayout { limit: 0, base_address: 0}});

static CPU_TSS: PerCpu<Spinlock<TaskStateSegment>> = PerCpu::new_with(
    [const {Spinlock::new(TaskStateSegment::create())}; MAX_CPUS]);

#[unsafe(no_mangle)]
pub extern "C" fn kern_addr_space_start() -> ! {
    info!("Switched to new address space");
    crate::cpu::set_panic_base(cpu::get_current_stack_base());
    crate::module::complete_handoff();
    
    info!("CPU-0 stack address:{:#X}", cpu::get_current_stack_base());
    init_tables();
    lapic::init();
    init_per_cpu_data(0);
    syscall::init();
    timer::init();
    handlers::init();
    
    enable_interrupts(true);

#[cfg(all(not(test), feature="acpi"))]
    smp::init();
    crate::kern_main();
}

pub fn set_tss_stack(stack_base: u64) {
    let mut cpu_tss = CPU_TSS.local().lock();
    cpu_tss.rsp0 = stack_base;
}

pub fn build_gdt() {
    // During BSP init, we will use the boot stack allocated by our assembly entry stub as the worker stack for cpu-0
    // According to current design, we have 4 primary stacks for cpu-0(bsp)
    // 1) The TSS stack which will be used for interrupt handlers running on cpu-0 during ring-3 to ring-0 transition
    // 2) Idle stack which is later created by scheduler which will be exclusively used by idle task
    // 3) The current stack which will continue to be used by the init task
    // 4) The backup/good stack, which will be used to run handlers for double fault and nmi exceptions, 
    // in case the primary stack gets corrupted (For ex: stack overflow)
    
    // Here, we will simply initialize the TSS to per cpu worker stack
    // However, it will be changed to the kernel stack for a given user thread by the scheduler 
    let tss_base = {
        let mut cpu_tss = CPU_TSS.local().lock();
        *cpu_tss = TaskStateSegment::new(cpu::get_current_stack_base() as u64, cpu::get_current_good_stack_base() as u64); 
        &*cpu_tss as *const _ as u64
    };

    let tss_desc =  TSSDescriptor::new(size_of::<TaskStateSegment>() as u64 - 1, tss_base);  
    
    let mut cpu_table = CPU_TABLE_DATA.local().lock();
    cpu_table.gdt_array = [
                // Current layout
                // Null segment + Kernel code + Kernel data + User data + User code
                // This layout is required for syscall/sysret to work
                // With this layout the segment selectors are as follows
                // Kernel code -> CS=0x8, Kernel data -> SS=0x10,
                // User code -> CS=0x23, User data -> SS=0x1B
                // TSS=0x28
                
                GDT::new(false, false, false, 0),
                GDT::new(true, true, true, 0),
                GDT::new(false, false, true, 0),
                GDT::new(false, false, true, 3),
                GDT::new(true, true, true, 3),
                tss_desc[0],
                tss_desc[1]
            ];
    
    cpu_table.gdt_layout.base_address = cpu_table.gdt_array.as_ptr() as u64;     
    cpu_table.gdt_layout.limit = (7 * size_of::<u64>() - 1) as u16;
    
    debug!("Setting gdt layout={:?}", cpu_table.gdt_layout);
}

pub fn register_tables() {
    let cpu_table = CPU_TABLE_DATA.local().lock();
    let idt = IDT_DATA.lock();
    debug!("Registering GDT and IDT");
    unsafe {
        asm::setup_table(ptr_to_usize(&cpu_table.gdt_layout) as u64, ptr_to_usize(&idt.idt_layout) as u64);
    }
}

fn init_tables() {
    // Build the IDT
    {
        let mut idt = IDT_DATA.lock();
        debug!("Interrupt stub address for vector 0 -> {:#X}", asm::IDT_TABLE[0] as u64);

        for vector in 0..MAX_INTERRUPT_VECTORS {
            let idt_desc = IDTDescriptor::new(KERNEL_CODE_SELECTOR as u64, asm::IDT_TABLE[vector] as u64,
            vector == super::DOUBLE_FAULT_VECTOR || vector == super::NMI_FAULT_VECTOR);
            idt.idt[vector * 2] = idt_desc[0];
            idt.idt[vector * 2 + 1] = idt_desc[1]; 
        }

        idt.idt_layout.base_address = idt.idt.as_ptr() as u64;     
        idt.idt_layout.limit = (MAX_INTERRUPT_VECTORS * 2 * size_of::<u64>() - 1) as u16;
        
        debug!("Setting idt layout={:?}", idt.idt_layout);
    }
    
    build_gdt();
    register_tables();
}
