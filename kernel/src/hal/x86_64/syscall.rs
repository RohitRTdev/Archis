use kernel_intf::debug;
use crate::hal::{disable_interrupts, enable_interrupts, get_per_cpu_base, get_per_cpu_kernel_base, set_per_cpu_base, set_tss_stack};
use crate::hal::x86_64::cpu_regs::INIT_RFLAGS;
use crate::sched::{get_current_task, set_kernel_mode_and_syscall_params, syscall_dispatcher, toggle_cur_task_kernel_mode};
use crate::hal::x86_64::asm;

const STAR: u32 = 0xc000_0081;
const LSTAR: u32 = 0xc000_0082;
const CSTAR: u32 = 0xc000_0083;
const SFMASK: u32 = 0xc000_0084;

#[cfg(test)]
static SYSCALL_ENTRY: u8 = 0; 

#[cfg(not(test))]
unsafe extern "C" {
    static SYSCALL_ENTRY: u8;
}

pub const MAX_ARCH_ARGS: usize = 6;

#[repr(C)]
struct SyscallContext {
    user_rsp: u64,
    syscall_number: u64,
    args: [u64; MAX_ARCH_ARGS],
    user_gs: u64
}

#[unsafe(no_mangle)]
extern "C" fn arch_syscall_handler(context: *mut SyscallContext) -> i64 {
    // With current design, we need to preserve the invariant that under kernel mode execution
    // a thread shall always have kernel_gs_base = user_gs_base
    let per_cpu_base = get_per_cpu_base();
    let user_rsp = unsafe {
        (*context).user_gs = per_cpu_base;
        (*context).user_rsp
    };

    set_per_cpu_base(get_per_cpu_kernel_base());
    set_kernel_mode_and_syscall_params(true, true, per_cpu_base, user_rsp);

    enable_interrupts(true);

    let (syscall_number, args) = unsafe {
        ((*context).syscall_number, &(*context).args)
    };

    let stat = syscall_dispatcher(syscall_number, args);

    disable_interrupts();
    set_kernel_mode_and_syscall_params(false, false, per_cpu_base, user_rsp);
    
    // Restore user gs
    unsafe {
        set_per_cpu_base((*context).user_gs);
    }

    stat
}

pub fn init() {
    let cstar: u64 = 0;
    let lstar: u64 = unsafe {
        &SYSCALL_ENTRY as *const u8 as u64
    };
    
    // Mask the TF=8, IF=9 and DF=10 flag
    let sfmask: u64 = 0b111 << 8;
    
    // sysret cs = 0x13 = (hardware uses this entry + 0x8 as user SS and +0x10 as user CS in GDT, RPL = 3),
    // syscall cs = 0x8 =  (hardware uses this entry as kernel CS and +0x8 as kernel SS, RPL = 0)
    let star: u64 = (0x13 << 48) | (0x8 << 32);

    debug!("Init syscall registers with lstar={:#X}, star={:#X}", lstar, star);

    unsafe {
        asm::wrmsr(SFMASK, sfmask);
        asm::wrmsr(CSTAR, cstar);
        asm::wrmsr(LSTAR, lstar);
        asm::wrmsr(STAR, star);
    }        
}

pub fn transfer_control_to_user(user_start_addr: usize, user_stack_base: usize) {
    toggle_cur_task_kernel_mode();
    set_tss_stack(get_current_task()
        .expect("transfer_control_to_user() called in idle task!")
        .lock()
        .get_stack()
        .expect("User thread expected to have non-empty kernel stack!") as u64
    );
    
    // This will be reenabled once switched to user land
    disable_interrupts();
    unsafe {
        let rflags = INIT_RFLAGS;

        asm::jump_to_user_code(user_start_addr as u64, rflags, user_stack_base as u64);
    }
    
    panic!("Exec path returned to transfer_control_to_user!!");
}