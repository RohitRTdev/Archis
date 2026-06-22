#![cfg_attr(not(test), no_std)]

use core::ptr::null_mut;
use kernel_intf::{
    get_module_name, info,
    sched_create_process, sched_create_thread, sched_delay_ms,
    sched_exit_process, sched_exit_thread, sched_get_cur_process_arg,
    sched_get_cur_thread_arg, sched_get_num_process_args, sched_wait_process,
};

#[repr(C)]
struct TestStruct {
    val1: usize,
    val2: isize,
}

extern "C" fn worker_thread() -> ! {
    let tid = kernel_intf::sched_get_cur_thread_id();
    info!("test1: worker_thread started (tid={})", tid);
    sched_delay_ms(500);
    info!("test1: worker_thread done (tid={})", tid);
    sched_exit_thread(0);
}

#[kmod::test_function(true)]
fn test_fn() {
    info!("Hi! Testing kernel module test framework!");
}


#[kmod::init]
fn module_init() {
    let num_args = sched_get_num_process_args();
    info!("Starting module {} with {} arg(s)", get_module_name!(), num_args);

    // Arg 0 is always the image name; print arg 1 if provided.
    if num_args > 1 {
        info!("Arg 1 is {}", unsafe { sched_get_cur_process_arg(1).as_str() });
    }

    // Dereference the context pointer only when it was provided (non-null).
    let ctx_ptr = sched_get_cur_thread_arg();
    if !ctx_ptr.is_null() {
        let test_arg = unsafe { &mut *(ctx_ptr as *mut TestStruct) };
        info!("Got thread argument: val1 = {}, val2 = {}", test_arg.val1, test_arg.val2);
        test_arg.val1 = 10;
    } else {
        info!("No thread argument (context_ptr is null)");
    }

    // Spawn a worker thread in this process and let it run concurrently.
    let tid = sched_create_thread(worker_thread, null_mut()).unwrap();
    info!("test1: spawned worker thread (tid={})", tid);

    // Only the "root" invocation (more than one arg) spawns a child process of
    // itself, which prevents infinite recursion.
    if num_args > 1 {
        info!("test1: spawning child process libtest1.so");
        let child_pid = sched_create_process(&["libtest1.so"], null_mut()).unwrap();
        info!("test1: child process spawned (pid={})", child_pid);

        // Wait for the child to finish before exiting.
        if child_pid != usize::MAX {
            sched_wait_process(child_pid);
            info!("test1: child process {} finished", child_pid);
        }
    }

    info!("test1: waiting for worker thread to finish");
    sched_delay_ms(1000);

    info!("test1: exiting process with code 1");
    sched_exit_process(1);
}
