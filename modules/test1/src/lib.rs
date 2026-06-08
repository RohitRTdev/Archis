#![cfg_attr(not(test), no_std)]

use kernel_intf::{get_module_name, info, sched_delay_ms, sched_exit_process, sched_get_cur_process_arg, sched_get_cur_thread_arg, sched_get_num_process_args};


#[kmod::init]
fn module_init() {
    info!("Starting module... {} with args {:?}", get_module_name!(), sched_get_num_process_args());

    info!("Arg 1 is {}", unsafe { sched_get_cur_process_arg(1).as_str() });

    let test_arg = unsafe { &mut *(sched_get_cur_thread_arg() as *mut TestStruct) };
    #[repr(C)]
    struct TestStruct {
        val1: usize,
        val2: isize
    }

    info!("Got thread argument: val1 = {}, val2 = {}", test_arg.val1, test_arg.val2);
    test_arg.val1 = 10;

    info!("Waiting for 5 seconds");
    sched_delay_ms(5000);

    info!("Exiting process..");
    sched_exit_process(1);
}