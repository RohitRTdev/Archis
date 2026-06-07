use core::alloc::Layout;
use core::ffi::c_void;
use common::PAGE_SIZE;
use kernel_intf::{KError, info};
use crate::cpu::Stack;
use crate::hal::{MAX_ARCH_ARGS, copy_user_memory, transfer_control_to_user};
use crate::mem::{PageDescriptor, allocate_memory};
use super::*;
use kernel_intf::*;

#[cfg(test)]
static USER_FN_START: u8 = 0;

#[cfg(test)]
static USER_FN_END: u8 = 0;


#[cfg(not(test))]
unsafe extern "C" {
    static USER_FN_START: u8;
    static USER_FN_END: u8;
}


const MAX_SYSCALLS: usize = 6;

static SYSCALL_TABLE: [fn(&[u64; MAX_ARCH_ARGS]) -> i64; MAX_SYSCALLS] = [
    sys_exit_handler,
    sys_thread_exit_handler,
    sys_write_handler,
    sys_delay_handler,
    sys_thread_handler,
    sys_process_handler
];


// Must be called from valid process context
pub fn create_user_thread(handler: DispatchRoutine, context: *mut c_void) -> Result<KThread, KError> {
    let res = create_thread_do_work(user_init_handler, Some(handler), core::ptr::null_mut());

    if res.is_err() {
        info!("User thread creation failed!");
    }

    res
}

pub extern "C" fn user_init_handler() -> ! {
    // Create the user stack
    // Allocate the user memory range for init handler
    // Transfer control to user

    let mut stack = Stack::new_user_stack().expect("Failed to create user stack!");
    info!("Created new user stack with base:{:#X}", stack.get_stack_base()); 
    add_memory_range_to_cur_process(stack.get_alloc_base(), stack.get_stack_size(), true);
    
    // User stacks will be cleaned up by the process manager, so remove ownership
    let stack_base = Stack::into_inner(&mut stack).addr().get();

    let user_fn_top = unsafe {
        &USER_FN_START as *const u8 as usize
    };
    
    let user_fn_last = unsafe {
        &USER_FN_END as *const u8 as usize
    };

    let user_stub_size = user_fn_last - user_fn_top;

    let user_stub_base = allocate_memory(Layout::from_size_align(user_stub_size, PAGE_SIZE).unwrap(), PageDescriptor::VIRTUAL 
    | PageDescriptor::USER).expect("User stub allocation failed!");

    info!("Allocated user stub at addr: {:#X} with size {}", user_stub_base.addr(), user_stub_size);
    unsafe {
        copy_user_memory(user_stub_base, &USER_FN_START as *const u8, user_stub_size);
    }

    add_memory_range_to_cur_process(user_stub_base.addr(), user_stub_size, true);

    // Let parent process know that user init is complete
    // Ensure that this process is dropped beyond this block since we won't return to this function
    {
        let process = get_current_process().expect("This shouldn't have happened??");
        process.lock().complete_init();
    }

    // Now transfer control to user thread
    info!("Transferring control to user function at address:{:#X} with stack:{:#X}", user_stub_base.addr(), stack_base); 
    transfer_control_to_user(user_stub_base.addr(), stack_base);

    panic!("Should not reach user_init_handler end!");
}


pub fn syscall_dispatcher(syscall_number: u64, syscall_args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    if syscall_number as usize >= MAX_SYSCALLS {
        return E_INVALID;
    } 

    SYSCALL_TABLE[syscall_number as usize](syscall_args)
}

fn sys_exit_handler(_args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let id = get_current_process_id().expect("Called sys_exit_handler from idle task!");

    kill_process(id, 0);

    E_SUCCESS
}

fn sys_thread_exit_handler(_args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let id = get_current_task_id().expect("Called sys_thread_exit_handler from idle task!");

    kill_thread(id, 0);

    E_SUCCESS
}

// Arg1 = pointer to string, arg2 = length of string
fn sys_write_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    //let mut str_buf = vec![0u8; args[1] as usize];
    //let str_buf_ptr = str_buf.as_mut_ptr();

    ////let mut str_buf: [u8; 128] = [0; 128];

    //let user_str_raw = unsafe {
    //    copy_user_memory(str_buf.as_mut_ptr() as *mut u8, args[0] as *const u8, args[1] as usize);
    //    let bytes = core::slice::from_raw_parts(str_buf_ptr, args[1] as usize);
    //    str::from_utf8(bytes)
    //};

    //if let Ok(user_str) = user_str_raw {
    //    kernel_intf::println!("{}", user_str);    
    //    E_SUCCESS
    //}
    //else {
    //    E_INVALID
    //}

    info!("Hello world!");

    E_SUCCESS
}

// Arg 1 = delay in ms
fn sys_delay_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    debug!("Delaying thread");
    delay_ms(args[0] as usize);

    E_SUCCESS
}

extern "C" fn placeholder_user_entry() -> ! {
    loop {}
}

fn sys_thread_handler(_args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    info!("Creating new user thread..");
    let stat: KError = create_user_thread(placeholder_user_entry, core::ptr::null_mut()).into();

    stat.into()
}

fn sys_process_handler(_args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let stat: KError = create_process(alloc::vec![], core::ptr::null_mut(), true).into();

    stat.into()
}
