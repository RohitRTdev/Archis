use core::ffi::c_void;
use core::mem::size_of;
use core::alloc::Layout;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use common::{PAGE_SIZE, align_up};
use crate::cpu::Stack;
use crate::hal::{MAX_ARCH_ARGS, copy_user_memory, transfer_control_to_user};
use crate::loader::load_user_image;
use crate::mem::{self, PageDescriptor};
use crate::sched;
use crate::sched::Handle::{ProcessHandle, ThreadHandle};
use super::*;
use kernel_intf::*;

const MAX_SYSCALLS: usize = 18;
const PROCESS_SUSPENDED_FLAG: u64 = 1 << 0;

static SYSCALL_TABLE: [fn(&[u64; MAX_ARCH_ARGS]) -> i64; MAX_SYSCALLS] = [
    sys_exit_handler,
    sys_thread_exit_handler,
    sys_read_handler,
    sys_write_handler,
    sys_close_handler,
    sys_open_file_handler,
    sys_open_device_handler,
    sys_delay_handler,
    sys_create_thread_handler,
    sys_create_process_handler,
    sys_resume_process_handler,
    sys_set_session_leader_handler,
    sys_get_pid_handler,
    sys_get_process_info_handler,
    sys_allocate_memory_handler,
    sys_deallocate_memory_handler,
    sys_set_signal_handler,
    sys_sigreturn_handler
];

fn read_c_strlen(start: usize) -> Option<usize> {
    let mut len = 0;
    let mut cur_ptr = start;
    let mut cur_range = align_up(start, PAGE_SIZE) - start;
    if cur_range == 0 {
        cur_range = PAGE_SIZE;
    }

    let mut pages_checked = 0;
    loop {
        let mut buf = vec![0u8; cur_range];
        if mem::copy_from_user(buf.as_mut_ptr(), cur_ptr, cur_range).is_err() {
            debug!("User range: {:#X} with size {} invalid", cur_ptr, cur_range);
            return None;
        }

        for &b in buf.iter() {
            if b == 0 {
                return Some(len);
            }
            len += 1;
        }

        pages_checked += 1;
        if pages_checked >= 2 {
            return None;
        }

        cur_ptr += cur_range;
        cur_range = PAGE_SIZE;
    }
}

// Must be called from valid process context
pub fn create_user_thread(user_fn_addr: usize, context: *mut c_void) -> Result<KThread, KError> {
    if user_fn_addr == 0 {
        return Err(KError::InvalidArgument);
    }

    // The user VA is smuggled through the task CB's user_fn slot;
    // user_thread_init_handler pulls it back out before entering user mode
    let user_fn: DispatchRoutine = unsafe { core::mem::transmute(user_fn_addr) };
    let res = create_thread_do_work(None, user_thread_init_handler, Some(user_fn), context);

    if res.is_err() {
        info!("User thread creation failed!");
    }

    res
}

// Creates the user stack for the current thread and hands ownership of its
// physical range to the process. Returns the stack top (VA)
fn setup_user_stack() -> usize {
    let stack = Stack::new_user_stack().expect("Failed to create user stack!");
    debug!("Created new user stack with base:{:#X}", stack.get_stack_base());
    let stack_base = stack.get_stack_base();
    add_user_stack_to_cur_task(stack);
    
    stack_base
}

// Lays out process arguments on the user stack, main(argc, argv) style:
//
//  high │ arg strings (NUL terminated, packed)
//       │ argv[argc] = NULL
//       │ argv[argc-1] ... argv[0]   (user VAs of the strings)
//  rsp →│ argc
//
// The module entry is extern "C" fn() -> !, so the user-side runtime reads
// argc at [rsp] and argv at rsp + 8. Returns the adjusted (16-byte aligned) rsp
#[cfg(target_arch = "x86_64")]
fn push_args_to_user_stack(stack_top: usize, args: &[String]) -> usize {
    let argc = args.len();
    let strings_len: usize = args.iter().map(|a| a.len() + 1).sum();
    let ptrs_len = (argc + 1) * size_of::<usize>();   // + NULL terminator
    let block_len = size_of::<usize>() + ptrs_len + strings_len;

    // aligning down is the right move here since stack grows down
    let rsp = (stack_top - block_len) & !0xF;
    let block_size = stack_top - rsp;

    // Stage the whole block in kernel memory, then copy out in one shot
    let mut block = vec![0u8; block_size];
    block[0..size_of::<usize>()].copy_from_slice(&argc.to_ne_bytes());

    let mut str_cursor = size_of::<usize>() + ptrs_len;
    for (idx, arg) in args.iter().enumerate() {
        let ptr_off = size_of::<usize>() * (idx + 1);
        let user_str_addr = rsp + str_cursor;
        block[ptr_off..ptr_off + size_of::<usize>()].copy_from_slice(&user_str_addr.to_ne_bytes());

        block[str_cursor..str_cursor + arg.len()].copy_from_slice(arg.as_bytes());
        // NUL terminator is already zero from vec init
        str_cursor += arg.len() + 1;
    }

    unsafe {
        copy_user_memory(rsp as *mut u8, block.as_ptr(), block_size);
    }

    rsp
}

pub extern "C" fn user_init_handler() -> ! {
    let stack_top = setup_user_stack();

    // Everything heap-allocated must drop before transfer_control_to_user —
    // it never returns, so anything still live here leaks
    let (entry, rsp, is_suspended) = {
        let args: Vec<String> = {
            let proc = get_current_process().expect("user_init_handler called outside process context!");
            let guard = proc.lock();
            guard.get_args().to_vec()
        };

        // create_process guarantees args[0] exists; it names the module to run
        let load_res = load_user_image(&args[0]);

        let load_info = match load_res {
            Ok(info) => info,
            Err(e) => {
                info!("Failed to load user module '{}': {:?}", args[0], e);

                // The parent is blocked on init_notify — unblock it before dying
                {
                    let proc = get_current_process().unwrap();
                    proc.lock().complete_init(false);
                }
                exit_process(-1);
            }
        };

        let rsp = push_args_to_user_stack(stack_top, &args);
        let entry = load_info.lock().user().entry;
        add_new_handle(Handle::ImgHandle(load_info));

        // Let parent process know that user init is complete
        let is_suspended = {
            let proc = get_current_process().expect("This shouldn't have happened??");
            proc.lock().complete_init(true);
            proc.lock().get_status() == ProcessStatus::Suspended
        };


        (entry, rsp, is_suspended)
    };
    
    if is_suspended {
        sched::suspend_process();
    }

    debug!("Transferring control to user module entry:{:#X} with stack:{:#X}", entry, rsp);
    transfer_control_to_user(entry, rsp);

    panic!("Should not reach user_init_handler end!");
}

pub extern "C" fn user_thread_init_handler() -> ! {
    let stack_top = setup_user_stack();

    let (user_fn_addr, context) = {
        let task = get_current_task().expect("user_thread_init_handler called outside task context!");
        let guard = task.lock();
        let user_fn = guard.get_user_fn().expect("user_thread_init_handler called without a user function!");
        (user_fn as usize, guard.get_arg_context() as usize)
    };

    // Place the context pointer as the first item on the user stack; the user
    // function reads it from [rsp]
    #[cfg(target_arch = "x86_64")]
    let rsp = (stack_top - 16) & !0xF;
    unsafe {
        copy_user_memory(rsp as *mut u8, &context as *const usize as *const u8, size_of::<usize>());
    }

    debug!("Starting user thread at:{:#X} with context:{:#X} and stack:{:#X}", user_fn_addr, context, rsp);
    transfer_control_to_user(user_fn_addr, rsp);

    panic!("Should not reach user_thread_init_handler end!");
}


pub fn syscall_dispatcher(syscall_number: u64, syscall_args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    if syscall_number as usize >= MAX_SYSCALLS {
        return E_INVALID;
    } 

    SYSCALL_TABLE[syscall_number as usize](syscall_args)
}

fn sys_exit_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let id = get_current_process_id().expect("Called sys_exit_handler from idle task!");

    kill_process(id, args[0] as isize);

    E_SUCCESS
} 

fn sys_thread_exit_handler(_args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let id = get_current_task_id().expect("Called sys_thread_exit_handler from idle task!");

    kill_thread(id, 0);

    E_SUCCESS
}

// arg0 = fd
fn sys_close_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    if !remove_handle(args[0] as usize) {
        E_INVALID
    }
    else {
        E_SUCCESS
    }
}

// arg0 = fd, arg1 = user buffer, arg2 = length, arg3 = ptr to bytes written
fn sys_read_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    E_INVALID
}

// arg0 = pointer to string
fn sys_write_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    if args[0] == 0 {
        return E_INVALID;
    }

    let res = read_c_strlen(args[0] as usize);
    if res.is_none() {
        return E_INVALID_MEMORY_RANGE; 
    }
    let len = res.unwrap();
    if len == 0 {
        return E_INVALID;
    }

    let mut str_buf = vec![0u8; len];
    if mem::copy_from_user(str_buf.as_mut_ptr(), args[0] as usize, len).is_err() {
        return E_INVALID_MEMORY_RANGE;
    }

    let user_str_raw = core::str::from_utf8(&str_buf);
    match user_str_raw {
        Ok(s) => {
            info!("{}", s);
            E_SUCCESS
        }
        Err(_) => E_INVALID
    }
}

fn sys_open_device_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    E_INVALID
}

fn sys_open_file_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    E_INVALID
}

// arg 1 = delay in ms
fn sys_delay_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    delay_ms(args[0] as usize, true);

    E_SUCCESS
}

// arg1 = user VA of the thread function (extern "C" fn() -> !), arg2 = context pointer
fn sys_create_thread_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    info!("Creating new user thread..");
    let res = create_user_thread(args[0] as usize, args[1] as *mut c_void);
    match res {
        Ok(user_thread) => {
            return add_new_handle(ThreadHandle(user_thread)) as i64;
        },
        Err(err) => {
            return err.into();
        }
    }
}

// arg0 = command list ptr, arg1 = length of command list
// arg2 = flags
fn sys_create_process_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let len = args[1] as usize;
    if args[0] == 0 || len == 0 {
        return E_INVALID;
    }
    
    let mut command_args: Vec<*const i8> = vec![core::ptr::null(); len];
    // Validate and copy the pointer list
    if mem::copy_from_user(command_args.as_mut_ptr() as *mut u8, args[0] as usize, len * size_of::<usize>()).is_err() {
        debug!("Failed user range!start:{:#X}, size:{}", args[0] as usize, len);
        return E_INVALID_MEMORY_RANGE;
    }

    let mut command_args_str = Vec::new();
    for (id, &s) in command_args.iter().enumerate() {
        let command_c_str_res = read_c_strlen(s.addr());
        if command_c_str_res.is_none() {
            debug!("Failed for string idx {}", id);
            return E_INVALID_MEMORY_RANGE;
        }
        let command_c_str_len = command_c_str_res.unwrap();
        let mut command_str = vec![0u8; command_c_str_len];

        // Copy the string
        if mem::copy_from_user(command_str.as_mut_ptr(), s.addr(), command_c_str_len).is_err() {
            debug!("Failed to copy string idx {}", id);
            return E_INVALID_MEMORY_RANGE;
        }

        let path = match String::from_utf8(command_str) {
            Ok(s) => s,
            Err(_) => return E_INVALID
        };

        command_args_str.push(path);
    }

    let is_suspended = args[2] & PROCESS_SUSPENDED_FLAG != 0;
    let res = create_process(
        command_args_str, 
        core::ptr::null_mut(), 
        true,
        is_suspended
    );

    match res {
        Ok(user_process) => {
            return add_new_handle(ProcessHandle(user_process)) as i64;
        },
        Err(err) => {
            return err.into();
        }
    }
}

// arg0 = pid
fn sys_resume_process_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    // We are not able to distinguish between cases where a process just got killed
    // vs when a process never existed at all. So, we just send same error code in 
    // both those cases
    if resume_process(args[0] as usize) {
        E_SUCCESS
    }
    else {
        E_PROCESS_TERMINATED
    }
}

// arg0 = pid
fn sys_set_session_leader_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let res = get_process_info(args[0] as usize);
    if res.is_none() {
        return E_PROCESS_TERMINATED;
    }

    if proc::set_session_leader(args[0] as usize) {
        E_SUCCESS
    }
    else {
        E_NOPERM
    }
}

fn sys_get_pid_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let cur_proc_id = get_current_process_id().expect("syscall in idle process??");
    cur_proc_id as i64
}

// arg0 = proc_fd, arg1 = process info ptr
fn sys_get_process_info_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let res = get_handle(args[0] as usize);
    if res.is_none() {
        return E_INVALID;
    }

    if let ProcessHandle(this_proc) = res.unwrap() {
        let info = this_proc.lock().get_process_header();
        if mem::copy_to_user(args[1] as usize, &info as *const _ as *const u8, size_of::<ProcessInfo>()).is_err() {
            return E_INVALID_MEMORY_RANGE;
        }

        E_SUCCESS
    }
    else {
        E_PROCESS_TERMINATED
    }
}

// arg0 = size, arg1 = ptr to result
fn sys_allocate_memory_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let layout = match Layout::from_size_align(args[0] as usize, PAGE_SIZE) {
        Ok(l) => l,
        Err(_) => return E_INVALID,
    };
    let res = mem::allocate_memory(layout, PageDescriptor::VIRTUAL | PageDescriptor::USER);

    match res {
        Ok(virt_addr) => {
            if mem::copy_to_user(args[1] as usize, &virt_addr as *const _ as *const u8, size_of::<usize>()).is_err() {
                return E_INVALID_MEMORY_RANGE;
            }

            E_SUCCESS
        },
        _ => {
            E_OOM
        }
    }
}

// arg0 = addr, arg1 = size
fn sys_deallocate_memory_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let size = args[1] as usize;
    if size == 0 {
        return E_INVALID;
    }

    let layout = match Layout::from_size_align(size, PAGE_SIZE) {
        Ok(l) => l,
        Err(_) => return E_INVALID,
    };

    let res = mem::deallocate_memory(args[0] as *mut u8, layout, PageDescriptor::VIRTUAL | PageDescriptor::USER);
    match res {
        Ok(..) => {
            E_SUCCESS
        },
        _ => {
            E_INVALID
        }
    }
}

// arg0 = signal (u8), arg1 = handler fn ptr (user VA), arg2 = user_ctx ptr
fn sys_set_signal_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let signal = args[0] as u8;
    if (signal as usize) >= MAX_SIGNALS {
        return E_INVALID;
    }

    let handler_addr = args[1] as usize;
    if handler_addr == 0 {
        return E_INVALID;
    }

    let handler: DispatchRoutine = unsafe { core::mem::transmute(handler_addr) };
    let user_ctx = args[2] as *mut c_void;

    let proc = get_current_process().expect("sys_set_signal_handler called from idle task");
    proc.lock().set_signal_handler(signal, SignalHandler {
        user_ctx,
        handler
    });

    debug!("sys_set_signal_handler: signal={} handler={:#X} user_ctx={:#X}",
        signal, handler_addr, args[2]);

    E_SUCCESS
}

fn sys_sigreturn_handler(_args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    complete_signal();
    E_SUCCESS
}