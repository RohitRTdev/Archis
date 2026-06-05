#![cfg_attr(not(test), no_std)]
#![feature(generic_const_exprs)]
#![feature(likely_unlikely)]
#![feature(allocator_api)]
#![feature(box_as_ptr)]

mod infra;
mod hal;
mod sync;
mod mem;
mod logger;
mod cpu;
mod devices;
mod sched;
mod fs;
mod loader;
mod io;

#[cfg(feature = "acpi")]
mod acpica;

use core::sync::atomic::{AtomicBool, Ordering};

use kernel_intf::{info, debug};
use common::*;
use loader::module;

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;


#[cfg(test)]
mod tests;

use sync::{Once, Spinlock};
use cpu::install_interrupt_handler;
use hal::read_port_u8;
use mem::Regions::*;
use mem::FixedList;
use kernel_intf::driver::{IrpMajor, IrpMinor, IrpResult, create_device_by_id};
use kernel_intf::list::{List, DynList};
use sched::KThread;
use sync::KSem;

use crate::sync::KEvent;

static BOOT_INFO: Once<BootInfo> = Once::new();

#[derive(Debug)]
enum RemapType {
    IdentityMapped,
    OffsetMapped(fn(usize))
}

#[derive(Debug)]
struct RemapEntry {
    value: MemoryRegion,
    map_type: RemapType,
    flags: u8
}

const KERNEL_PATH: &'static str = "/sys/aris";

struct InitFS {
    fs: BTreeMap<&'static str, &'static [u8]>,
    symlinks: BTreeMap<&'static str, &'static str>
}

static INIT_FS: Once<InitFS> = Once::new();  
static REMAP_LIST: Spinlock<FixedList<RemapEntry, {Region2 as usize}>> = Spinlock::new(List::new());

fn clear_keyboard_output_buffer() {
    unsafe {
        while read_port_u8(0x64) & 0x01 != 0 {
            let _ = read_port_u8(0x60);
        }
    }
}

static TASK_COUNTER: Once<KSem> = Once::new();

extern "C" fn spawned_task_entry() -> ! {
    let id = sched::get_current_task_id().unwrap();
    info!("Running task: {}", id);
    TASK_COUNTER.get().unwrap().signal();

    loop {
        info!("id:{}", id);
        sched::delay_ms(1000);
    }
}

// Checking thread subsystem
extern "C" fn task_spawn() -> ! {
    let mut tasks: VecDeque<KThread> = VecDeque::new();
    TASK_COUNTER.call_once(|| {
        KSem::new(0, 5)
    });

    let task_id = sched::get_current_task_id().unwrap();
    info!("Starting task spawner, id={}", task_id);

    for idx in 0..5 {
        info!("Creating task {} in task spawner", idx);
        tasks.push_back(sched::create_thread(spawned_task_entry, core::ptr::null_mut()).unwrap());
    }

    info!("Task spawner going to wait!");
    
    for _ in 0..5 {
        TASK_COUNTER.get().unwrap().wait().unwrap();
    }

    info!("Task spawner starting kill spree");

    loop {
        info!("Task spawner waiting for keyboard event");
        KEYBOARD_EVENT.get().unwrap().wait().unwrap();
        info!("Task spawner springing to action");
        if !tasks.is_empty() {
            let task = tasks.pop_front().unwrap();
            let id = task.lock().get_id();
            info!("Killing task {}", id);
            sched::kill_thread(id, 0);
        }
        else {
            info!("Killing self");
            sched::exit_thread(0);
        }
    }
}

extern "C" fn process_spawn() -> ! {
    for _ in 0..2 {
        sched::create_process(alloc::vec!["/test_proc".into()], core::ptr::null_mut(), false)
            .expect("Failed to create process");
    }

    // This pattern should be never followed in a real scenario, but this is here just for testing
    let sem = KSem::new(0, 1);

    info!("Init Thread going to wait state");
    let _ = sem.wait();

    // This is here in case this process is killed before it gets a chance to wait forever
    sched::exit_thread(0);
}

static QUEUE: Spinlock<DynList<[i64; 64]>> = Spinlock::new(List::new());

extern "C" fn thread_creator() -> ! {
    let id = sched::get_current_task_id().unwrap();
    let mut counter = 1;
    loop {
        sched::delay_ms(1000);
        debug!("Running thread with id {}", id);
        sched::create_thread(thread_creator, core::ptr::null_mut()).expect("Failed to create child thread!");
        if counter % 5 == 0 {
            sched::create_process(alloc::vec!["/test_proc".into()], core::ptr::null_mut(), false)
                .expect("Failed to create child process!");
        }

        if counter % 10 == 0 {
            sched::exit_process(0);
        }
        counter += 1;
    }
}

// === IRP cancellation tests ===
//   (a) task-kill cancellation — kill_thread → kill_sweep_irps walks the
//       dying thread's IRP list and invokes the driver's cancel routine.
//   (b) per-handle cancellation — issuing thread itself calls
//       cancel_pending_irp on the device handle. Same cancel chain, just
//       not driven by task death.
//   (c) duplicate device name guard — io_create_device must return null
//       when the name is already in DEVICE_BY_NAME.

extern "C" fn cancel_test_completion(result: *const IrpResult, _ctx: *mut core::ffi::c_void) {
    let status = unsafe { (*result).status };
    info!("cancel_test_completion: IRP delivered with status {}", status as isize);
}

fn task_kill_cancel_runner() -> ! {
    let handle = match io::open_device_handle("test1") {
        Ok(h) => h,
        Err(_) => {
            info!("task_kill_cancel_runner: test1 device not found");
            sched::exit_thread();
        }
    };

    info!("task_kill_cancel_runner: issuing sync read");
    let _ = handle.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: 0, size: 0 },
        offset: 0,
    });

    // If kill_thread fired during the wait, kill_sweep_irps cancelled the
    // IRP, the driver's cancel routine completed it, and the event was
    // signalled with status=Cancelled. The thread won't make it here in
    // practice (it's already TERMINATED).
    info!("task_kill_cancel_runner: read returned (post-cancel)");
    loop { sched::delay_ms(1000); }
}

fn self_cancel_runner() -> ! {
    let handle = match io::open_device_handle("test1") {
        Ok(h) => h,
        Err(_) => {
            info!("self_cancel_runner: test1 device not found");
            sched::exit_thread();
        }
    };

    info!("self_cancel_runner: dispatching async read");
    let _ = io::io_request_async(
        &handle,
        IrpMajor::Read,
        IrpMinor::None,
        MemoryRegion { base_address: 0, size: 0 },
        0,
        cancel_test_completion,
        core::ptr::null_mut(),
    );

    // Give the driver a moment to register its cancel routine and start
    // waiting in the worker, then cancel.
    sched::delay_ms(200);
    info!("self_cancel_runner: calling cancel_pending_irp");
    io::cancel_pending_irp(&handle);

    // Let cancellation settle (driver worker wakes after 1500ms in test1).
    sched::delay_ms(2000);
    info!(
        "self_cancel_runner: pending_irps on test1 = {}",
        handle.pending_irps.lock().get_nodes()
    );
    sched::exit_thread();
}

fn run_cancel_tests() {
    // (a) task-kill cancellation
    let killer_target = sched::create_thread(task_kill_cancel_runner)
        .expect("Failed to spawn task_kill_cancel_runner");
    sched::delay_ms(300);
    let tid = killer_target.lock().get_id();
    info!("cancel test (a): killing thread {}", tid);
    sched::kill_thread(tid);
    // Wait long enough that the driver worker hits io_start_processing and
    // exits cleanly (1500ms sleep in test1::read_worker + slack).
    sched::delay_ms(2500);

    // (b) per-handle self-cancellation
    let _ = sched::create_thread(self_cancel_runner)
        .expect("Failed to spawn self_cancel_runner");
    sched::delay_ms(3000);

    // (c) duplicate device name guard. test1 was named "test1" at add time.
    let probe = match io::open_device_handle("test1") {
        Ok(h) => h,
        Err(_) => {
            info!("cancel test (c): SKIP — test1 device missing");
            return;
        }
    };
    let driver_id = unsafe { (*probe.device_ptr()).get_driver_id() };
    let dup = create_device_by_id(driver_id, Some("test1"), core::ptr::null_mut(), None);
    info!("cancel test (c): duplicate create returned {:#X}", dup.addr());
    assert!(dup.is_null(), "duplicate device name 'test1' must be rejected");
    info!("cancel test (c): PASSED");
}

fn interative_thread_spawn_tests() {
    KEYBOARD_EVENT.call_once(|| {
        KEvent::new(true)
    });

    // Sample invocation to test out interrupt subsystem
    clear_keyboard_output_buffer();
    install_interrupt_handler(1, key_notifier, true, true);
    
    sched::create_thread(watchdog).unwrap();
    let spawn_proc = sched::create_process(process_spawn, false).expect("Failed to create second process");
    info!("Main task waiting for process id 1 to complete");
    spawn_proc.wait().expect("Unable to wait on process id 1");
    
    let spawn_task = sched::create_thread(task_spawn).unwrap();

    info!("Main task waiting for task id 1 to complete");
    spawn_task.wait().expect("Unable to wait on task id 1");
}

fn spam_threads_test() {
    let user_proc0 = sched::create_process(|| -> ! {loop{}}, true)
    .expect("Failed to create user process 0");
    
    sched::create_thread(thread_creator).expect("Failed to create kernel thread!");
}

fn producer_consumer_test() {
    sched::create_thread(|| {
        loop {
            {
                let mut queue = QUEUE.lock();
                queue.add_node([0; 64]).expect("Failed to add node from producer!");
                let addr = queue.last().unwrap().as_ptr().addr();
                debug!("Added new node at address {:#X}", addr);
            }
            sched::delay_ms(1000);
        }
    }).expect("Failed to create producer thread!");

    sched::create_process(|| {
       loop {
            {
                let mut queue = QUEUE.lock();
                let node = queue.first();
                if node.is_some() {
                    let node = node.unwrap();
                    debug!("[Consumer]: Found node at address: {:#X}", node.as_ptr().addr());
                }
                queue.pop_node();
            }
            sched::delay_ms(1000);
       } 
    }, false).expect("Failed to create consumer process!");
}

fn kern_main() -> ! {
    info!("Starting main kernel init");

    sched::init();
    loader::init();
    io::init();

    run_cancel_tests();

    loop {
        sched::delay_ms(1000);
        info!("Main task looping");
    }

    //hal::sleep();
}

// This will be called from the entry point for the corresponding arch
// For now, we support only x86_64, so the entry point is at kernel/src/hal/x86_64/asm/kernel_entry.S
#[unsafe(no_mangle)]
unsafe extern "C" fn kern_start(boot_info: *const BootInfo) -> ! {
    BOOT_INFO.call_once(|| {
        unsafe { *boot_info }
    });   

    mem::setup_heap();
    logger::init();

    info!("Starting aris");
    devices::init();
    cpu::init();
    module::early_init();
    
    debug!("{:?}", *BOOT_INFO.get().unwrap());

    hal::init();
}

static KEYBOARD_EVENT: Once<KEvent> = Once::new();

fn key_notifier(_: usize) {
    let stack_base = hal::get_current_stack_base();
    info!("Interrupt stack base = {:#X}", stack_base);
    let avl_memory = mem::get_available_memory();
    info!("Available memory: {}", avl_memory);
    let task = sched::get_current_task();
    if task.is_none() {
        info!("Called keyboard handler from idle task on core {}", hal::get_core());
    }
    else {
        let task = task.unwrap();
        let id = task.lock().get_id();
        let status = task.lock().get_status();
        info!("Called keyboard handler in task:{} with status: {:?}", id, status);
    }
    
    KEYBOARD_EVENT.get().unwrap().signal();
    clear_keyboard_output_buffer();

    // Let the watchdog task know that we're active
    WATCHDOG_MARK.store(true, Ordering::Release);
}

static WATCHDOG_MARK: AtomicBool = AtomicBool::new(false);

extern "C" fn watchdog() -> ! {
    loop {
        sched::delay_ms(10_000);
        let is_active = WATCHDOG_MARK.load(Ordering::Acquire);
        
        if !is_active {
            info!("Watchdog task clearing keyboard buffer");
            clear_keyboard_output_buffer();
            WATCHDOG_MARK.store(true, Ordering::Release);
        }
        else {
            WATCHDOG_MARK.store(false, Ordering::Release);
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn exported_function() {
    info!("Driver called exported function!");
}