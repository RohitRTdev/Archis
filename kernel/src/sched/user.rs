use core::ffi::c_void;
use core::mem::size_of;
use core::alloc::Layout;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use common::{PAGE_SIZE, MemoryRegion, align_up};
use crate::cpu::Stack;
use crate::hal::{MAX_ARCH_ARGS, copy_user_memory, get_time_ms, transfer_control_to_user};
use crate::devices::read_realtime;
use crate::loader::load_user_image;
use crate::mem::{self, PageDescriptor};
use crate::io::io_request_sync;
use crate::pipe::{PipeType, create_named_pipe};
use crate::sched::Handle::{PipeReadHandle, PipeWriteHandle};
use crate::sched::{self, Handle::{FileHandle, ProcessHandle, SyncHandle, ThreadHandle, DeviceHandle}};
use crate::fs::{FileBuffer, FileStat, HandleStatType};
use crate::sync::{KEvent, KSem, do_signal, do_wait};
use super::*;
use kernel_intf::*;
use kernel_intf::driver::{IrpMajor, IrpMinor, ReqInfo, TtyControlInfo, TtyModeInfo, EMPTY_REGION, Status};

const MAX_SYSCALLS: usize = 39;
const PROCESS_SUSPENDED_FLAG: u64 = 1 << 0;

const OPEN_INHERITABLE_FLAG: u64 = 1 << 0;
pub const OPEN_CREATE_FLAG: u64 = 1 << 1;
pub const OPEN_WRITE_FLAG: u64 = 1 << 2;

const SYNC_TYPE_SEMAPHORE: u64 = 0;
const SYNC_TYPE_EVENT: u64 = 1;

const CREATE_FILE_EXIST_FLAG: u64 = 1 << 1;

const SEEK_SET: u64 = 0;
const SEEK_CUR: u64 = 1;
const SEEK_END: u64 = 2;


static SYSCALL_TABLE: [fn(&[u64; MAX_ARCH_ARGS]) -> i64; MAX_SYSCALLS] = [
    sys_exit_handler,
    sys_thread_exit_handler,
    sys_read_handler,
    sys_write_handler,
    sys_close_handler,
    sys_open_handler,
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
    sys_sigreturn_handler,
    sys_create_sync_object_handler,
    sys_wait_handler,
    sys_signal_handler,
    sys_get_time_ms_handler,
    sys_duplicate_handler,
    sys_create_pgrp_handler,
    sys_get_tid_handler,
    sys_get_thread_info_handler,
    sys_device_control_handler,
    sys_seek_handler,
    sys_fstat_handler,
    sys_readdir_handler,
    sys_delete_file_handler,
    sys_rename_file_handler,
    sys_mkdir_handler,
    sys_rmdir_handler,
    sys_create_file_handler,
    sys_create_symlink_handler,
    sys_readlink_handler,
    sys_create_pipe_handler,
    sys_chdir_handler,
    sys_getcwd_handler
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

// Lays out process arguments and environment on the user stack,
// main(argc, argv, envp) style:
//
//  high │ argv strings (NUL terminated, packed)
//       │ envp strings (NUL terminated, packed)
//       │ envp[envc] = NULL
//       │ envp[envc-1] ... envp[0]   (user VAs of the strings)
//       │ argv[argc] = NULL
//       │ argv[argc-1] ... argv[0]   (user VAs of the strings)
//  rsp →│ argc
//
// The module entry is extern "C" fn() -> !, so the user-side runtime 
// reads argc at [rsp], argv at rsp + 8, and derives envp as argv + (argc + 1) * 8 
// Returns the adjusted (16-byte aligned) rsp
#[cfg(target_arch = "x86_64")]
fn push_args_and_envp_to_user_stack(stack_top: usize, args: &[String], envp: &[String]) -> usize {
    let argc = args.len();
    let envc = envp.len();

    let argv_strings_len: usize = args.iter().map(|a| a.len() + 1).sum();
    let envp_strings_len: usize = envp.iter().map(|a| a.len() + 1).sum();

    let argv_ptrs_len = (argc + 1) * size_of::<usize>();   // + NULL terminator
    let envp_ptrs_len = (envc + 1) * size_of::<usize>();   // + NULL terminator

    let header_len = size_of::<usize>() + argv_ptrs_len + envp_ptrs_len;
    let block_len = header_len + argv_strings_len + envp_strings_len;

    // aligning down is the right move here since stack grows down
    let rsp = (stack_top - block_len) & !0xF;
    let block_size = stack_top - rsp;

    // Stage the whole block in kernel memory, then copy out in one shot
    let mut block = vec![0u8; block_size];
    block[0..size_of::<usize>()].copy_from_slice(&argc.to_ne_bytes());

    let argv_ptrs_off = size_of::<usize>();
    let envp_ptrs_off = argv_ptrs_off + argv_ptrs_len;
    let mut str_cursor = header_len;

    for (idx, arg) in args.iter().enumerate() {
        let ptr_off = argv_ptrs_off + size_of::<usize>() * idx;
        let user_str_addr = rsp + str_cursor;
        block[ptr_off..ptr_off + size_of::<usize>()].copy_from_slice(&user_str_addr.to_ne_bytes());

        block[str_cursor..str_cursor + arg.len()].copy_from_slice(arg.as_bytes());
        // NUL terminator is already zero from vec init
        str_cursor += arg.len() + 1;
    }
    // argv[argc] = NULL is already zero from vec init

    for (idx, e) in envp.iter().enumerate() {
        let ptr_off = envp_ptrs_off + size_of::<usize>() * idx;
        let user_str_addr = rsp + str_cursor;
        block[ptr_off..ptr_off + size_of::<usize>()].copy_from_slice(&user_str_addr.to_ne_bytes());

        block[str_cursor..str_cursor + e.len()].copy_from_slice(e.as_bytes());
        // NUL terminator is already zero from vec init
        str_cursor += e.len() + 1;
    }
    // envp[envc] = NULL is already zero from vec init

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
        let (args, envp): (Vec<String>, Vec<String>) = {
            let proc = get_current_process().expect("user_init_handler called outside process context!");
            let guard = proc.lock();
            (guard.get_args().to_vec(), guard.get_envp().to_vec())
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
                exit_process(ExitInfo::normal(-1));
            }
        };

        let rsp = push_args_and_envp_to_user_stack(stack_top, &args, &envp);
        let entry = load_info.lock().user().entry;

        // Let parent process know that user init is complete
        let is_suspended = {
            let proc = get_current_process().expect("This shouldn't have happened??");
            proc.lock().set_image(load_info);
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

    // Place the context pointer at [rsp - 8]
    #[cfg(target_arch = "x86_64")]
    assert!(stack_top & 0xF == 0, "user stack base must be 16-byte aligned");
    #[cfg(target_arch = "x86_64")]
    let rsp = stack_top;
    unsafe {
        copy_user_memory((rsp - size_of::<usize>()) as *mut u8, &context as *const usize as *const u8, size_of::<usize>());
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

    kill_process(id, ExitInfo::normal(args[0] as isize));

    E_SUCCESS
}

fn sys_thread_exit_handler(_args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let id = get_current_task_id().expect("Called sys_thread_exit_handler from idle task!");

    kill_thread(id, ExitInfo::normal(0));

    E_SUCCESS
}

// arg0 = handle
fn sys_close_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    if !remove_handle(args[0] as usize) {
        E_INVALID
    }
    else {
        E_SUCCESS
    }
}

// arg0 = handle, arg1 = user buf ptr, arg2 = len, arg3 = ptr to bytes read
fn sys_read_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let len = args[2] as usize;
    if len == 0 {
        return E_SUCCESS;
    }
    let buf = FileBuffer::from(args[1] as usize, len, true);
    match get_handle(args[0] as usize) {
        Some(FileHandle(f)) => {
            let completed = match f.read(&buf) {
                Ok(n) => n,
                Err(e) => return e.into()
            };
            if mem::copy_to_user(args[3] as usize, &completed as *const usize as *const u8, size_of::<usize>()).is_err() {
                return E_INVALID_MEMORY_RANGE;
            }
            E_SUCCESS
        },
        Some(PipeReadHandle(p)) => {
            let completed = match p.read(&buf) {
                Ok(n) => n,
                Err(e) => return e.into()
            };
            
            if mem::copy_to_user(args[3] as usize, &completed as *const usize as *const u8, size_of::<usize>()).is_err() {
                return E_INVALID_MEMORY_RANGE;
            }
            E_SUCCESS
        },
        Some(DeviceHandle(h)) => {
            let mut kbuf = vec![0u8; len];
            let region = MemoryRegion { base_address: kbuf.as_mut_ptr() as usize, size: len };
            
            // Tell driver to write to the kernel buffer
            let result = match io_request_sync(&**h, IrpMajor::Read, IrpMinor::None, region, 0, None, true) {
                Ok(r) => r,
                Err(e) => return e.into()
            };
            if result.status != Status::Success {
                return E_INVALID;
            }
            let completed = result.bytes_completed;

            // Now copy back to user
            if mem::copy_to_user(args[1] as usize, kbuf.as_ptr(), completed).is_err() {
                return E_INVALID_MEMORY_RANGE;
            }
            if mem::copy_to_user(args[3] as usize, &completed as *const usize as *const u8, size_of::<usize>()).is_err() {
                return E_INVALID_MEMORY_RANGE;
            }
            E_SUCCESS
        },
        _ => E_INVALID
    }
}

// arg0 = handle, arg1 = user buf ptr, arg2 = len, arg3 = ptr to bytes written
fn sys_write_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let len = args[2] as usize;
    if len == 0 {
        return E_SUCCESS;
    }
    let buf = FileBuffer::from(args[1] as usize, len, true);
    match get_handle(args[0] as usize) {
        Some(FileHandle(f)) => {
            let completed = match f.write(&buf, len, 0) {
                Ok(n) => n,
                Err(e) => return e.into()
            };
            if mem::copy_to_user(args[3] as usize, &completed as *const usize as *const u8, size_of::<usize>()).is_err() {
                return E_INVALID_MEMORY_RANGE;
            }
            E_SUCCESS
        },
        Some(PipeWriteHandle(p)) => {
            let completed = match p.write(&buf) {
                Ok(n) => n,
                Err(e) => return e.into()
            };
            if mem::copy_to_user(args[3] as usize, &completed as *const usize as *const u8, size_of::<usize>()).is_err() {
                return E_INVALID_MEMORY_RANGE;
            }
            E_SUCCESS
        },
        Some(DeviceHandle(h)) => {
            let mut kbuf = vec![0u8; len];
            if mem::copy_from_user(kbuf.as_mut_ptr(), args[1] as usize, len).is_err() {
                return E_INVALID_MEMORY_RANGE;
            }
            let region = MemoryRegion { base_address: kbuf.as_ptr() as usize, size: len };
            let result = match io_request_sync(&**h, IrpMajor::Write, IrpMinor::None, region, 0, None, false) {
                Ok(r) => r,
                Err(_) => return E_INVALID
            };
            if result.status != Status::Success {
                return E_INVALID;
            }
            let completed = result.bytes_completed;
            if mem::copy_to_user(args[3] as usize, &completed as *const usize as *const u8, size_of::<usize>()).is_err() {
                return E_INVALID_MEMORY_RANGE;
            }
            E_SUCCESS
        },
        _ => E_INVALID
    }
}

// arg0 = type C-string ptr, arg1 = name C-string ptr, arg2 = flags (bit 0 = IS_INHERITABLE)
fn sys_open_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    if args[0] == 0 || args[1] == 0 {
        return E_INVALID;
    }
    let type_str = match read_user_string(args[0] as usize) {
        Some(s) if !s.is_empty() => s,
        Some(_) => return E_INVALID,
        None => return E_INVALID_MEMORY_RANGE
    };
    let name_str = match read_user_string(args[1] as usize) {
        Some(s) if !s.is_empty() => s,
        Some(_) => return E_INVALID,
        None => return E_INVALID_MEMORY_RANGE
    };
    let is_inheritable = args[2] & OPEN_INHERITABLE_FLAG != 0;
    match crate::object::open_object(&type_str, &name_str, args[2]) {
        Ok(handle) => add_new_handle(handle, is_inheritable) as i64,
        Err(e) => e.into()
    }
}

fn read_user_string(ptr: usize) -> Option<String> {
    let len = read_c_strlen(ptr)?;
    if len == 0 {
        return Some(String::new());
    }
    let mut buf = vec![0u8; len];
    if mem::copy_from_user(buf.as_mut_ptr(), ptr, len).is_err() {
        return None;
    }
    String::from_utf8(buf).ok()
}

const MAX_ENVP_ENTRIES: usize = 256;

// Reads a NUL-pointer-terminated array of C string pointers from user memory
// stopping at the first NULL entry or MAX_ENVP_ENTRIES, whichever comes first.
// A NULL `ptr` (or a first entry that is NULL) yields an empty environment.
fn read_user_envp(ptr: usize) -> Option<Vec<String>> {
    if ptr == 0 {
        return Some(Vec::new());
    }

    let mut envp = Vec::new();
    for idx in 0..MAX_ENVP_ENTRIES {
        let mut entry: usize = 0;
        let entry_ptr = ptr + idx * size_of::<usize>();
        if mem::copy_from_user(&mut entry as *mut usize as *mut u8, entry_ptr, size_of::<usize>()).is_err() {
            return None;
        }
        if entry == 0 {
            return Some(envp);
        }

        let s = read_user_string(entry)?;
        envp.push(s);
    }

    Some(envp)
}

// arg0 = device handle, arg1 = minor code, arg2 = command (depends on the minor code)
fn sys_device_control_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let handle = match get_handle(args[0] as usize) {
        Some(DeviceHandle(h)) => h,
        _ => return E_INVALID
    };
    let minor = match IrpMinor::from_usize(args[1] as usize) {
        Some(m) => m,
        None => return E_INVALID
    };

    let req_info = match minor {
        // For SetForegroundPgrp and SetControllingTty, the command is the pid of
        // process whose process grp / session we want to set
        IrpMinor::SetForegroundPgrp | IrpMinor::SetControllingTty => {
            ReqInfo { tty_control: TtyControlInfo { pid: args[2] as usize } }
        },
        // For SetTtyMode, the command is the new mode bitmask
        IrpMinor::SetTtyMode => {
            ReqInfo { tty_mode: TtyModeInfo { mode: args[2] as u8 } }
        },
        // For GetTtyMode, the command is a user pointer to write the mode byte back into
        IrpMinor::GetTtyMode => {
            ReqInfo { tty_mode: TtyModeInfo { mode: 0 } }
        },
        _ => return E_INVALID
    };

    let result = match io_request_sync(&**handle, IrpMajor::Control, minor, EMPTY_REGION, 0, Some(req_info), false) {
        Ok(r) => r,
        Err(_) => return E_INVALID
    };
    if result.status != Status::Success {
        return E_NOPERM;
    }

    if minor == IrpMinor::GetTtyMode {
        let mode = unsafe { result.req_info.tty_mode }.mode;
        if mem::copy_to_user(args[2] as usize, &mode as *const u8, 1).is_err() {
            return E_INVALID_MEMORY_RANGE;
        }
    }

    E_SUCCESS
}

// arg 1 = delay in ms
fn sys_delay_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    if !delay_ms(args[0] as usize, true) {
        E_WAIT_INTERRUPTED
    }
    else {
        E_SUCCESS
    }
}

// arg1 = user VA of the thread function (extern "C" fn() -> !), arg2 = context pointer
fn sys_create_thread_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    info!("Creating new user thread..");
    let res = create_user_thread(args[0] as usize, args[1] as *mut c_void);
    match res {
        Ok(user_thread) => {
            return add_new_handle(ThreadHandle(user_thread), false) as i64;
        },
        Err(err) => {
            return err.into();
        }
    }
}

// arg0 = command list ptr, arg1 = length of command list
// arg2 = envp list ptr (NUL-pointer-terminated array; 0 = no environment)
// arg3 = flags
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

    let envp_str = match read_user_envp(args[2] as usize) {
        Some(v) => v,
        None => return E_INVALID_MEMORY_RANGE
    };

    let is_suspended = args[3] & PROCESS_SUSPENDED_FLAG != 0;
    let res = create_process(
        command_args_str.as_slice(),
        envp_str.as_slice(),
        core::ptr::null_mut(),
        true,
        is_suspended
    );

    match res {
        Ok(user_process) => {
            add_new_handle(ProcessHandle(user_process), false) as i64
        },
        Err(err) => {
            err.into()
        }
    }
}

// arg0 = process_handle
fn sys_resume_process_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let pid = match get_handle(args[0] as usize) {
        Some(h) => {
            match h {
                ProcessHandle(p) => { p.lock().get_id() },
                _ => { return E_INVALID; }
            }
        },
        None => { return E_INVALID; }
    };

    // We are not able to distinguish between cases where a process just got killed
    // vs when a process never existed at all. So, we just send same error code in 
    // both those cases
    if resume_process(pid) {
        E_SUCCESS
    }
    else {
        E_PROCESS_TERMINATED
    }
}

// arg0 = process_handle (-1 if current process)
fn sys_set_session_leader_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let pid = if args[0] as i64 == -1 {
        get_current_process_id().expect("syscall called from idle task!")
    }
    else {
        let res = get_handle(args[0] as usize);
        if res.is_none() {
            return E_INVALID;
        }

        match res.unwrap() {
            ProcessHandle(p) => {
                if p.lock().get_status() == ProcessStatus::Terminated {
                    return E_PROCESS_TERMINATED;
                }

                p.lock().get_id()
            },
            _ => { return E_INVALID; }
        }
    };

    if proc::set_session_leader(pid) {
        E_SUCCESS
    }
    else {
        E_NOPERM
    }
}

// arg0 = process_handle (-1 if current process)
fn sys_create_pgrp_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let pid = if args[0] as i64 == -1 {
        get_current_process_id().expect("syscall called from idle task!")
    }
    else {
        let res = get_handle(args[0] as usize);
        if res.is_none() {
            return E_INVALID;
        }

        match res.unwrap() {
            ProcessHandle(p) => {
                if p.lock().get_status() == ProcessStatus::Terminated {
                    return E_PROCESS_TERMINATED;
                }

                p.lock().get_id()
            },
            _ => { return E_INVALID; }
        }
    };

    if proc::set_pgroup_leader(pid) {
        E_SUCCESS
    }
    else {
        E_NOPERM
    }
}

fn sys_get_pid_handler(_args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let cur_proc_id = get_current_process_id().expect("syscall in idle process??");
    cur_proc_id as i64
}

fn sys_get_tid_handler(_args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let task = get_current_task().expect("sys_get_tid called from idle task!");
    task.lock().get_id() as i64
}

// arg0 = thread handle, arg1 = ptr to thread_info_t
fn sys_get_thread_info_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let res = get_handle(args[0] as usize);
    if let Some(ThreadHandle(thread)) = res {
        let info = {
            let guard = thread.lock();
            ThreadInfo {
                id: guard.get_id() as u64,
                exit_info: guard.get_exit_info()
            }
        };
        if mem::copy_to_user(args[1] as usize, &info as *const _ as *const u8, size_of::<ThreadInfo>()).is_err() {
            return E_INVALID_MEMORY_RANGE;
        }
        E_SUCCESS
    }
    else {
        E_INVALID
    }
}

// arg0 = proc_handle (-1 = current process), arg1 = process info ptr
fn sys_get_process_info_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    if args[0] as i64 == -1 {
        let proc = get_current_process().expect("sys_get_process_info_handler called from idle task!");
        let info = proc.lock().get_process_header();
        if mem::copy_to_user(args[1] as usize, &info as *const _ as *const u8, size_of::<ProcessInfo>()).is_err() {
            return E_INVALID_MEMORY_RANGE;
        }
        return E_SUCCESS;
    }

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
    let flags = PageDescriptor::VIRTUAL | PageDescriptor::USER; 
    let res = mem::allocate_memory(layout, flags);

    match res {
        Ok(virt_addr) => {
            if mem::copy_to_user(args[1] as usize, &virt_addr as *const _ as *const u8, size_of::<usize>()).is_err() {
                mem::deallocate_memory(virt_addr, layout, flags).expect("Unexpected failure during memory deallocation!");
                return E_INVALID_MEMORY_RANGE;
            }

            add_memory_range_to_cur_process(virt_addr.addr(), layout.size(), true);

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
            remove_memory_range_from_cur_process(args[0] as usize, size, true);
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
}

// arg0 = sync type, arg1 = init count, arg2 = max count, arg3 = auto reset, arg4 = is inheritable
// arg5 = name C-string ptr (0 or ptr to empty string = anonymous)
fn sys_create_sync_object_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    if args[0] == SYNC_TYPE_SEMAPHORE && args[2] == 0 {
        // max count cannot be zero
        return E_INVALID;
    }

    if args[0] == SYNC_TYPE_EVENT && args[3] != 0 && args[3] != 1 {
        // auto_reset must be 0 or 1
        return E_INVALID;
    }

    if args[4] != 0 && args[4] != 1 {
        return E_INVALID;
    }

    let obj = if args[0] == SYNC_TYPE_SEMAPHORE {
        KSem::new(args[1] as isize, args[2] as isize).inner()
    }
    else {
        KEvent::new(args[3] == 1).inner()
    };

    if args[5] != 0 {
        let name = match read_user_string(args[5] as usize) {
            Some(s) => s,
            None => return E_INVALID_MEMORY_RANGE
        };
        if !name.is_empty() {
            if let Err(e) = crate::sync::register_named_sync(&name, obj.clone()) {
                return e.into();
            }
        }
    }

    add_new_handle(SyncHandle(obj), args[4] == 1) as i64
}

// arg0 = waitable obj handle, arg1 = timeout
// if timeout = -1, then infinite timeout
fn sys_wait_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let timeout = if args[1] as i64 != -1 {
        Some(args[1] as usize)
    }
    else {
        None
    };
    let wait_sem = match get_handle(args[0] as usize) {
        Some(h) => {
            match h {
                ProcessHandle(h1) => { h1.get_inner_sem() },
                ThreadHandle(h2) => { h2.get_inner_sem() },
                SyncHandle(h3) => { h3.clone() },
                _ => { return E_INVALID; }
            }
        },
        None => { return E_INVALID; }
    };

    let res = do_wait(&wait_sem, timeout, true);

    match res {
        Ok(()) => { E_SUCCESS },
        Err(err) => { err.into() }
    }
}

// arg0 = waitable obj handle
fn sys_signal_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let wait_sem = match get_handle(args[0] as usize) {
        Some(h) => {
            match h {
                ProcessHandle(h1) => { h1.get_inner_sem() },
                ThreadHandle(h2) => { h2.get_inner_sem() },
                SyncHandle(h3) => { h3.clone() },
                _ => { return E_INVALID; }
            }
        },
        None => { return E_INVALID; }
    };

    do_signal(&wait_sem);
    E_SUCCESS
}

const CLOCK_MONOTONIC: u64 = 0;
const CLOCK_WALL_TIME: u64 = 1;

fn rtc_to_unix_ms(t: kernel_intf::RtcTime) -> u64 {
    // RTC year is 2-digit: 0 = 2000, 24 = 2024, etc.
    let year = 2000u64 + t.year as u64;

    // Leap years before `year` minus leap years before 1970 (= 477).
    // Formula: floors of y/4 - y/100 + y/400 counts leap years up to and including y.
    let leap_days = (year - 1) / 4 - (year - 1) / 100 + (year - 1) / 400 - 477;

    let days_to_year = (year - 1970) * 365 + leap_days;

    const MDAYS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut days_in_year: u64 = 0;
    for m in 0..(t.month as usize).saturating_sub(1) {
        days_in_year += MDAYS[m];
    }
    let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    if t.month > 2 && is_leap {
        days_in_year += 1;
    }

    let total_days = days_to_year + days_in_year + t.day as u64 - 1;
    let total_secs = total_days * 86400
        + t.hour as u64 * 3600
        + t.minute as u64 * 60
        + t.second as u64;
    total_secs * 1000
}

// arg0 = target_proc handle (-1 = current proc), arg1 = old handle (in current proc)
// arg2 = new handle in target proc (-1 = allocate new slot), arg3 = is_inheritable
fn sys_duplicate_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let target_proc_arg = args[0] as i64;
    let old_handle = args[1] as usize;
    let new_handle_arg = args[2] as i64;

    if args[3] != 0 && args[3] != 1 {
        return E_INVALID;
    }
    let is_inheritable = args[3] != 0;

    let handle_to_dup = match get_handle(old_handle) {
        Some(h) => h,
        None => return E_INVALID
    };

    if target_proc_arg == -1 {
        let cur_proc = get_current_process()
            .expect("sys_duplicate_handler called from idle task!");
        if new_handle_arg == -1 {
            add_handle_to_proc(&cur_proc, handle_to_dup, is_inheritable) as i64
        } else {
            place_handle_in_proc(&cur_proc, new_handle_arg as usize, handle_to_dup, is_inheritable);
            new_handle_arg
        }
    } else {
        let target_proc = match get_handle(target_proc_arg as usize) {
            Some(ProcessHandle(p)) => p,
            _ => return E_INVALID
        };

        if target_proc.lock().get_status() != ProcessStatus::Suspended {
            return E_NOPERM;
        }

        if new_handle_arg == -1 {
            add_handle_to_proc(&target_proc, handle_to_dup, is_inheritable) as i64
        } else {
            place_handle_in_proc(&target_proc, new_handle_arg as usize, handle_to_dup, is_inheritable);
            new_handle_arg
        }
    }
}

// arg0 = clock type, arg1 = pointer to uint64_t to receive milliseconds
fn sys_get_time_ms_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let ms = match args[0] {
        CLOCK_MONOTONIC => get_time_ms(),
        CLOCK_WALL_TIME => rtc_to_unix_ms(read_realtime()),
        _ => return E_INVALID
    };
    if mem::copy_to_user(args[1] as usize, &ms as *const u64 as *const u8, size_of::<u64>()).is_err() {
        return E_INVALID;
    }
    E_SUCCESS
}

// arg0 = file handle, arg1 = offset (i64), arg2 = whence
// Returns new file offset on success, or negative error
fn sys_seek_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let f = match get_handle(args[0] as usize) {
        Some(FileHandle(f)) => f,
        _ => return E_INVALID
    };
    let delta = args[1] as i64;
    let new_offset = match args[2] {
        SEEK_SET => {
            if delta < 0 { return E_INVALID; }
            delta as usize
        }
        SEEK_CUR => {
            let cur = f.get_offset() as i64;
            let next = cur + delta;
            if next < 0 { return E_INVALID; }
            next as usize
        }
        SEEK_END => {
            let end = f.len() as i64;
            let next = end + delta;
            if next < 0 { return E_INVALID; }
            next as usize
        }
        _ => return E_INVALID
    };
    match f.seek(new_offset) {
        Ok(()) => new_offset as i64,
        Err(e) => e.into()
    }
}

// arg0 = handle, arg1 = ptr to file_stat_t
fn sys_fstat_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let handle = match get_handle(args[0] as usize) {
        Some(h) => h,
        None => return E_INVALID
    };

    let stat = match handle {
        FileHandle(f) => {
            let attrs = f.fstat();
            FileStat { size: attrs.size, mode: attrs.mode, handle_type: HandleStatType::File }
        }
        DeviceHandle(_) => FileStat { size: 0, mode: 0, handle_type: HandleStatType::Device },
        ThreadHandle(_) => FileStat { size: 0, mode: 0, handle_type: HandleStatType::Thread },
        ProcessHandle(_) => FileStat { size: 0, mode: 0, handle_type: HandleStatType::Process },
        SyncHandle(_) => FileStat { size: 0, mode: 0, handle_type: HandleStatType::Sync },
        PipeReadHandle(_) => FileStat { size: 0, mode: 0, handle_type: HandleStatType::PipeRead },
        PipeWriteHandle(_) => FileStat { size: 0, mode: 0, handle_type: HandleStatType::PipeWrite }
    };

    if mem::copy_to_user(args[1] as usize, &stat as *const FileStat as *const u8, size_of::<FileStat>()).is_err() {
        return E_INVALID_MEMORY_RANGE;
    }
    E_SUCCESS
}

// arg0 = dir handle, arg1 = offset, arg2 = buf ptr, arg3 = buf len, arg4 = ptr to bytes written
fn sys_readdir_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let f = match get_handle(args[0] as usize) {
        Some(FileHandle(f)) => f,
        _ => return E_INVALID
    };
    let offset = args[1] as usize;
    let buf_len = args[3] as usize;
    let entry = match f.readdir_at(offset) {
        Ok(e) => e,
        Err(e) => return e.into()
    };
    let name = entry.name.as_bytes();
    if name.len() + 1 > buf_len {
        return E_BUF_TOO_SMALL;
    }
    if mem::copy_to_user(args[2] as usize, name.as_ptr(), name.len()).is_err() {
        return E_INVALID_MEMORY_RANGE;
    }
    // write NUL terminator
    let nul: u8 = 0;
    if mem::copy_to_user(args[2] as usize + name.len(), &nul as *const u8, 1).is_err() {
        return E_INVALID_MEMORY_RANGE;
    }
    let written = name.len() + 1;
    if mem::copy_to_user(args[4] as usize, &written as *const usize as *const u8, size_of::<usize>()).is_err() {
        return E_INVALID_MEMORY_RANGE;
    }
    E_SUCCESS
}

// arg0 = path ptr
fn sys_delete_file_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let path = match read_user_string(args[0] as usize) {
        Some(s) if !s.is_empty() => s,
        Some(_) => return E_INVALID,
        None => return E_INVALID_MEMORY_RANGE
    };
    match crate::fs::delete(&path) {
        Ok(()) => E_SUCCESS,
        Err(e) => e.into()
    }
}

// arg0 = from path ptr, arg1 = to path ptr
fn sys_rename_file_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let from = match read_user_string(args[0] as usize) {
        Some(s) if !s.is_empty() => s,
        Some(_) => return E_INVALID,
        None => return E_INVALID_MEMORY_RANGE
    };
    let to = match read_user_string(args[1] as usize) {
        Some(s) if !s.is_empty() => s,
        Some(_) => return E_INVALID,
        None => return E_INVALID_MEMORY_RANGE
    };
    match crate::fs::rename(&from, &to) {
        Ok(()) => E_SUCCESS,
        Err(e) => e.into()
    }
}

// arg0 = path ptr
fn sys_mkdir_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let path = match read_user_string(args[0] as usize) {
        Some(s) if !s.is_empty() => s,
        Some(_) => return E_INVALID,
        None => return E_INVALID_MEMORY_RANGE
    };
    match crate::fs::mkdir(&path, 0) {
        Ok(()) => E_SUCCESS,
        Err(e) => e.into()
    }
}

// arg0 = path ptr
fn sys_rmdir_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let path = match read_user_string(args[0] as usize) {
        Some(s) if !s.is_empty() => s,
        Some(_) => return E_INVALID,
        None => return E_INVALID_MEMORY_RANGE
    };
    match crate::fs::delete(&path) {
        Ok(()) => E_SUCCESS,
        Err(e) => e.into()
    }
}

// arg0 = path ptr, arg1 = flags 
fn sys_create_file_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let path = match read_user_string(args[0] as usize) {
        Some(s) if !s.is_empty() => s,
        Some(_) => return E_INVALID,
        None => return E_INVALID_MEMORY_RANGE
    };
    let flags = args[1];
    let file_exist_only = flags & CREATE_FILE_EXIST_FLAG != 0;
    let is_inheritable = flags & OPEN_INHERITABLE_FLAG != 0;
    match crate::fs::create_or_open(&path, file_exist_only) {
        Ok(inst) => add_new_handle(FileHandle(inst), is_inheritable) as i64,
        Err(e) => e.into()
    }
}

// arg0 = path ptr, arg1 = target ptr
fn sys_create_symlink_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let path = match read_user_string(args[0] as usize) {
        Some(s) if !s.is_empty() => s,
        Some(_) => return E_INVALID,
        None => return E_INVALID_MEMORY_RANGE
    };
    let target = match read_user_string(args[1] as usize) {
        Some(s) if !s.is_empty() => s,
        Some(_) => return E_INVALID,
        None => return E_INVALID_MEMORY_RANGE
    };
    match crate::fs::create_symlink(&path, &target) {
        Ok(()) => E_SUCCESS,
        Err(e) => e.into()
    }
}

// arg0 = path ptr, arg1 = buf ptr, arg2 = buf len, arg3 = ptr to bytes written
fn sys_readlink_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let path = match read_user_string(args[0] as usize) {
        Some(s) if !s.is_empty() => s,
        Some(_) => return E_INVALID,
        None => return E_INVALID_MEMORY_RANGE
    };
    let buf_len = args[2] as usize;
    let (_, target_opt) = match crate::fs::lstat(&path) {
        Ok(r) => r,
        Err(e) => return e.into()
    };
    let target = match target_opt {
        Some(t) => t,
        None => return E_INVALID
    };
    let bytes = target.as_bytes();
    if bytes.len() + 1 > buf_len {
        return E_BUF_TOO_SMALL;
    }
    if mem::copy_to_user(args[1] as usize, bytes.as_ptr(), bytes.len()).is_err() {
        return E_INVALID_MEMORY_RANGE;
    }
    let nul: u8 = 0;
    if mem::copy_to_user(args[1] as usize + bytes.len(), &nul as *const u8, 1).is_err() {
        return E_INVALID_MEMORY_RANGE;
    }
    let written = bytes.len() + 1;
    if mem::copy_to_user(args[3] as usize, &written as *const usize as *const u8, size_of::<usize>()).is_err() {
        return E_INVALID_MEMORY_RANGE;
    }
    E_SUCCESS
}

// arg0 = ptr to read handle, arg1 = ptr to write handle, arg2 = name, arg3 = is inheritable
fn sys_create_pipe_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    if args[0] == 0 || args[1] == 0 {
        return E_INVALID;
    }

    let mut name = None;
    if args[2] != 0 {
        let res = read_user_string(args[2] as usize);
        match res {
            Some(s) => {
                if !s.is_empty() {
                    name = Some(s);
                }
            },
            None => { return E_INVALID_MEMORY_RANGE; }
        }
    }

    match create_named_pipe(name) {
        Err(err) => { err.into() },
        Ok(pipe) => {
            let read_handle = PipeReadHandle(PipeType::new(pipe.clone(), true));
            let write_handle = PipeWriteHandle(PipeType::new(pipe, false));

            let read_h = add_new_handle(read_handle, args[3] == 1);
            let write_h = add_new_handle(write_handle, args[3] == 1); 

            if mem::copy_to_user(args[0] as usize, &read_h as *const usize as *const u8, size_of::<usize>()).is_err() {
                return E_INVALID_MEMORY_RANGE;
            }
            
            if mem::copy_to_user(args[1] as usize, &write_h as *const usize as *const u8, size_of::<usize>()).is_err() {
                return E_INVALID_MEMORY_RANGE;
            }

            E_SUCCESS
        }
    }
}

// arg0 = path ptr
fn sys_chdir_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let path = match read_user_string(args[0] as usize) {
        Some(s) if !s.is_empty() => s,
        Some(_) => return E_INVALID,
        None => return E_INVALID_MEMORY_RANGE
    };
    match crate::fs::chdir(&path) {
        Ok(()) => E_SUCCESS,
        Err(e) => e.into()
    }
}

// arg0 = buf ptr, arg1 = buf len, arg2 = ptr to bytes written
fn sys_getcwd_handler(args: &[u64; MAX_ARCH_ARGS]) -> i64 {
    let raw = sched::get_cwd();
    // Follow any symlinks in the path so the result is a truly canonical path,
    // not just the lexically-normalized path that was used to chdir into it.
    let canonical = crate::fs::resolve_symlink(&raw).unwrap_or(raw);

    let buf_len = args[1] as usize;
    let bytes = canonical.as_bytes();
    if bytes.len() + 1 > buf_len {
        return E_BUF_TOO_SMALL;
    }
    if mem::copy_to_user(args[0] as usize, bytes.as_ptr(), bytes.len()).is_err() {
        return E_INVALID_MEMORY_RANGE;
    }
    let nul: u8 = 0;
    if mem::copy_to_user(args[0] as usize + bytes.len(), &nul as *const u8, 1).is_err() {
        return E_INVALID_MEMORY_RANGE;
    }
    let written = bytes.len() + 1;
    if mem::copy_to_user(args[2] as usize, &written as *const usize as *const u8, size_of::<usize>()).is_err() {
        return E_INVALID_MEMORY_RANGE;
    }
    E_SUCCESS
}
