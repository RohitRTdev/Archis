use core::alloc::Layout;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::ptr::NonNull;
use common::PAGE_SIZE;
use kernel_intf::{KError, info};
use crate::hal::get_core;
use crate::infra::disable_early_panic_phase;
use crate::sync::Spinlock;
use crate::mem::{PageDescriptor, allocate_memory, deallocate_memory, map_memory};

pub const INIT_STACK_SIZE: usize  = PAGE_SIZE * 10;
pub const INIT_BOOT_CPU_STACK_SIZE: usize  = PAGE_SIZE * 20;
pub const INIT_GUARD_PAGE_SIZE: usize = PAGE_SIZE;
pub const WORKER_STACK_SIZE: usize = 5 * PAGE_SIZE;
pub const TOTAL_BOOT_STACK_SIZE: usize = INIT_BOOT_CPU_STACK_SIZE + INIT_GUARD_PAGE_SIZE;

static TOTAL_CPUS: AtomicUsize = AtomicUsize::new(1);

#[cfg(test)]
static KERNEL_STACK: u8 = 0;

#[cfg(test)]
static KERNEL_STACK_TOP: u8 = 0;


#[cfg_attr(target_arch = "x86_64", repr(align(4096)))]
struct KStackGood {
    stack: [u8; PAGE_SIZE]
}

static KERN_BACKUP_STACK: KStackGood = KStackGood {
    stack: [0; PAGE_SIZE]
};

pub struct Stack {
    guard_size: usize,
    stack_size: usize,
    base: NonNull<u8>,
    allocated: bool,
    user: bool
}

#[cfg(not(test))]
unsafe extern "C" {
    static KERNEL_STACK: u8;
    static KERNEL_STACK_TOP: u8;
}


impl Stack {
    const fn create() -> Self {
        Self {
            guard_size: 0,
            stack_size: 0,
            base: NonNull::dangling(),
            allocated: false,
            user: false
        }
    }

    // Create STACK + GUARD page. The guard page will remain unmapped
    // This is to allow us to catch any stack overflow scenarios
    pub fn new() -> Result<Self, KError> {
        Self::new_with(INIT_STACK_SIZE, INIT_GUARD_PAGE_SIZE, false)
    }
    
    pub fn new_user_stack() -> Result<Self, KError> {
        Self::new_with(INIT_STACK_SIZE, INIT_GUARD_PAGE_SIZE, true)
    }
    
    pub fn new_with(stack_size: usize, guard_size: usize, is_user: bool) -> Result<Self, KError> {
        let vflags = if is_user {
            PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC | PageDescriptor::USER
        }
        else {
            PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC
        };

        let stack_raw = allocate_memory(Layout::from_size_align(stack_size + guard_size, PAGE_SIZE).unwrap()
        , vflags)?;
        
        let stack_raw_phys = match allocate_memory(Layout::from_size_align(stack_size, PAGE_SIZE).unwrap(), 0) {
            Ok(addr) => { addr },
            Err(err) => {
                deallocate_memory( 
                    stack_raw, 
                    Layout::from_size_align(stack_size + guard_size, PAGE_SIZE).unwrap(), 
                    vflags
                ).expect("Unexpected failure during roll back of memory allocation on stack creation!");

                return Err(err);
            }
        };

        #[cfg(feature = "stack_down")]
        let stack_base = unsafe {
            stack_raw.add(guard_size)
        };

        #[cfg(not(feature = "stack_down"))]
        let stack_base = stack_raw;
        let flags = if is_user {
            PageDescriptor::USER
        }
        else {
            PageDescriptor::VIRTUAL
        };

        match map_memory(stack_raw_phys.addr(), stack_base.addr(), stack_size, flags) {
            Err(err) => {
                // First deallocate the physical memory associated with the stack
                deallocate_memory(
                    stack_raw_phys, 
            Layout::from_size_align(stack_size, PAGE_SIZE).unwrap(),
                    0
                ).expect("Unexpected failure during deallocation of physical memory on stack creation!");
                
                // Now remove the virtual memory reserved for this stack
                deallocate_memory( 
                    stack_raw, 
                    Layout::from_size_align(stack_size + guard_size, PAGE_SIZE).unwrap(), 
                    vflags
                ).expect("Unexpected failure during roll back of memory allocation on stack creation during map!");

                return Err(err);
            },
            _ => {}
        }
    
        Ok(Self {guard_size, stack_size, base: NonNull::new(stack_raw).unwrap(), 
        allocated: true, user: is_user })
    }

    pub fn get_stack_size(&self) -> usize {
        self.guard_size + self.stack_size
    }

    pub fn into_inner(stack: &mut Stack) -> NonNull<u8> {
        assert!(stack.allocated == true);
        stack.allocated = false;

        NonNull::new(stack.get_stack_base() as *mut u8).unwrap() 
    }

    fn destroy(&mut self) {
        if !self.allocated {
            return;
        }

        let (flags, vflags)  = if self.user {
            (PageDescriptor::VIRTUAL | PageDescriptor::USER,
                PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC | PageDescriptor::USER)
        }
        else {
            (PageDescriptor::VIRTUAL,
                PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC)
        };

        crate::sched_log!("Destroying stack with alloc_base={:#X} and base={:#X}", self.get_alloc_base(), self.get_stack_base());
        deallocate_memory(self.get_stack_top() as *mut u8,
        Layout::from_size_align(self.stack_size, PAGE_SIZE).unwrap(),
        flags)
        .expect("Stack base address wrong during unmap??");
        
        // Deallocate the guard page memory (if any)
        if self.guard_size != 0 {
            deallocate_memory(
                self.get_alloc_base() as *mut u8,
                Layout::from_size_align(self.guard_size, PAGE_SIZE).unwrap()
            , vflags)
            .expect("Failed to deallocate memory for stack");
        }

        self.allocated = false;
    }
    
    #[cfg(feature = "stack_down")]
    #[inline(always)]
    pub fn get_alloc_base(&self) -> usize {
        self.base.as_ptr().addr()
    }
    
    #[cfg(not(feature = "stack_down"))]
    #[inline(always)]
    pub fn get_alloc_base(&self) -> usize {
        self.base.as_ptr().addr() + self.stack_size 
    }
    
    #[cfg(feature = "stack_down")]
    #[inline(always)]
    pub fn get_stack_base(&self) -> usize {
        self.base.as_ptr().addr() + self.guard_size + self.stack_size
    }

    #[cfg(not(feature = "stack_down"))]
    #[inline(always)]
    pub fn get_stack_base(&self) -> usize {
        self.base.as_ptr().addr()
    }
    
    #[cfg(feature = "stack_down")]
    #[inline(always)]
    pub fn get_stack_top(&self) -> usize {
        self.base.as_ptr().addr() + self.guard_size
    }

    #[cfg(not(feature = "stack_down"))]
    #[inline(always)]
    pub fn get_stack_top(&self) -> usize {
        self.base.as_ptr().addr() + self.stack_size
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl Default for Stack {
    fn default() -> Self {
        Stack::create()
    }
}

struct CPUControlBlock {
    worker_stack: Stack,
    good_stack: Stack,
    panic_base: usize
}

unsafe impl Send for CPUControlBlock {}

pub const MAX_CPUS: usize = 64; 

static CPU_ID: AtomicUsize = AtomicUsize::new(0);
static CPU_LIST: PerCpu<Spinlock<CPUControlBlock>> = PerCpu::new_with(
    [const {Spinlock::new(CPUControlBlock{worker_stack: Stack::create(), 
    good_stack: Stack::create(), panic_base: 0})}; MAX_CPUS]);

pub fn init() {
    register_cpu();
}

pub fn get_total_cores() -> usize {
    TOTAL_CPUS.load(Ordering::Acquire)
}

#[allow(dead_code)]
pub fn set_total_cores(total_count: usize) {
    TOTAL_CPUS.store(total_count, Ordering::Release);
}

// This function is to be called only by the BSP in order to register the AP's and itself into the system
pub fn register_cpu() -> usize {
    let cpu_id = CPU_ID.fetch_add(1, Ordering::Relaxed);
    assert!(cpu_id < TOTAL_CPUS.load(Ordering::Acquire));

    let cb = if cpu_id == 0 {
        
        // We will be using this stack set up from the assembly stub till we switch address spaces
        let boot_stack = unsafe {
            &KERNEL_STACK as *const u8 as *mut u8
        };

        let boot_stack_top = unsafe {
            &KERNEL_STACK_TOP as *const u8 as usize
        };

        let stack = Stack {
            stack_size: PAGE_SIZE * 5,
            guard_size: 0,
            base: NonNull::new(boot_stack).unwrap(),
            allocated: false,
            user: false
        };
 
        CPUControlBlock {
            worker_stack: stack, 
            good_stack: Stack {
                stack_size: PAGE_SIZE,
                guard_size: 0,
                base: NonNull::new(KERN_BACKUP_STACK.stack.as_ptr() as *mut u8).unwrap(),
                allocated: true,
                user: false
            },
            panic_base: boot_stack_top
        }
    } else {
        // Allocate worker stack for the CPU
        let stack = Stack::new_with(WORKER_STACK_SIZE, INIT_GUARD_PAGE_SIZE, false).expect("Failed to allocate memory for CPU worker stack");
        let backup_stack = Stack::new_with(PAGE_SIZE, 0, false).expect("Failed to create backup stack for cpu");
        let stack_base = stack.get_stack_base();

        CPUControlBlock {
            worker_stack: stack,
            good_stack: backup_stack,
            panic_base: stack_base
        }
    };

    info!("Registered CPU with core_id:{}, with stack:{:#X}, good_stack:{:#X}", cpu_id,
     cb.worker_stack.get_stack_base(), cb.good_stack.get_stack_base());

    unsafe {
        *CPU_LIST.get(cpu_id).lock() = cb;
    }

    disable_early_panic_phase();
    cpu_id
}

pub fn get_worker_stack(core_id: usize) -> usize {
    let cpu_list = unsafe {
        CPU_LIST.get(core_id).lock()
    };

    cpu_list.worker_stack.get_stack_base()
}

// This should be called once memory manager is up
pub fn set_worker_stack_for_boot_cpu(stack_base: *mut u8) {
    let stack = Stack {stack_size: INIT_BOOT_CPU_STACK_SIZE, guard_size: INIT_GUARD_PAGE_SIZE, 
        base: NonNull::new(stack_base).unwrap(), allocated: true, user: false};

    let mut cpu_list = CPU_LIST.local().lock();

    cpu_list.worker_stack = stack;
}

pub fn get_current_stack_base() -> usize {
    let cpu_list = CPU_LIST.local().lock();

    cpu_list.worker_stack.get_stack_base()
}

pub fn get_current_good_stack_base() -> usize {
    let cpu_list = CPU_LIST.local().lock();

    cpu_list.good_stack.get_stack_base()
}

pub fn get_panic_base() -> usize {
    let cpu_list = CPU_LIST.local().lock();

    cpu_list.panic_base
}

pub fn set_panic_base(base: usize) {
    let mut cpu_list = CPU_LIST.local().lock();

    cpu_list.panic_base = base;
}

// Usual cacheline size
#[repr(align(64))]
pub struct PerCpu<T: Sync> {
    pub data: [T; MAX_CPUS],
}

unsafe impl<T: Sync> Sync for PerCpu<T> {}

impl<T: Sync> PerCpu<T> {
    pub const fn new_with(init: [T; MAX_CPUS]) -> Self {
        Self { data: init }
    }
}

impl<T: Sync> PerCpu<T> {
    #[inline(always)]
    pub fn local(&self) -> &T {
        let cpu = get_core();
        &self.data[cpu]
    }

    // Caller must ensure correctness.
    #[inline(always)]
    pub unsafe fn get(&self, cpu: usize) -> &T {
        &self.data[cpu]
    }
}

#[unsafe(no_mangle)]
extern "C" fn get_core_ffi() -> usize {
    get_core()
}