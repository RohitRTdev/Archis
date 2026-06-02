#![cfg_attr(not(test), no_std)]
#![feature(generic_const_exprs)]
#![feature(likely_unlikely)]
#![feature(allocator_api)]

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
use kernel_intf::list::{List, DynList};
use sched::KThread;
use sync::KSem;

use crate::fs::FilePath;

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
static WAIT_EVENT: Once<KSem> = Once::new();
static WAIT_EVENT2: Once<KSem> = Once::new();

// Checking thread subsystem
fn task_spawn() -> ! {
    let mut tasks: VecDeque<KThread> = VecDeque::new();
    TASK_COUNTER.call_once(|| {
        KSem::new(0, 5)
    });

    WAIT_EVENT.call_once(|| {
        KSem::new(0, 1)
    });

    let task_id = sched::get_current_task_id().unwrap();
    info!("Starting task spawner, id={}", task_id);


    for idx in 0..5 {
        info!("Creating task {} in task spawner", idx);
        tasks.push_back(sched::create_thread(|| {
            let id = sched::get_current_task_id().unwrap(); 
            info!("Running task: {}", id);
            TASK_COUNTER.get().unwrap().signal();

            info!("id={}", id);
            
            loop {
                info!("id:{}", id);
            
                sched::delay_ms(1000);
            }
        }).unwrap());
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
            sched::kill_thread(id);
        }
        else {
            info!("Killing self");
            sched::exit_thread();
        }
    }
}

fn process_spawn() -> ! {
    for _ in 0..2 {
        sched::create_process(|| {
            let proc_id = sched::get_current_process_id().unwrap();
            let thread_id = sched::get_current_task_id().unwrap();
            info!("Created process with id {}", proc_id);  

            for _ in 0..2 {
                sched::create_thread(|| {
                    let thread_id = sched::get_current_task_id().unwrap();
                    let proc_id = sched::get_current_process_id().unwrap();
                    info!("Created new thread with id {}", thread_id);

                    loop {
                        info!("Running thread with id {} with process id {} on core {}", thread_id, proc_id, hal::get_core());
                        sched::delay_ms(1000);
                    }
                }).expect("Failed to create new thread");
            }

            loop {
                info!("Running thread with id {} with process id {}", thread_id, proc_id);
                info!("Process {} waiting for event..", proc_id);
                KEYBOARD_EVENT.get().unwrap().wait().unwrap();
                
                // Kill process 1 and then kill self
                sched::kill_process(1);
                sched::exit_process();
            }
        }, false).expect("Failed to create process");
    }

    // This pattern should be never followed in a real scenario, but this is here just for testing
    let sem = KSem::new(0, 1);

    info!("Init Thread going to wait state");
    let _ = sem.wait();

    // This is here incase this process is killed before it gets a chance to wait forever
    sched::exit_thread();

    // Just to appease the type system
    hal::halt();
}

static QUEUE: Spinlock<DynList<[i64; 64]>> = Spinlock::new(List::new());

fn thread_creator() -> ! {
    let id = sched::get_current_task_id().unwrap();
    let mut counter = 1;
    loop {
        sched::delay_ms(1000);
        debug!("Running thread with id {}", id);
        sched::create_thread(thread_creator).expect("Failed to create child thread!");
        if counter % 5 == 0 {
            sched::create_process(thread_creator, false).expect("Failed to create child process!");
        }
        
        if counter % 10 == 0 {
            sched::exit_process();
        }
        counter += 1;
    }
}

fn kern_main() -> ! {
    info!("Starting main kernel init");
    
    KEYBOARD_EVENT.call_once(|| {
        KSem::new(0, 1)
    });

    // Sample invocation to test out interrupt subsystem
    clear_keyboard_output_buffer();
    install_interrupt_handler(1, key_notifier, true, true);

    sched::init();
    loader::init();
    io::init();
    io::submit_read();

    // Some tests just to test out process and thread subsystem
    //{
    //    let spawn_proc = sched::create_process(process_spawn, false).expect("Failed to create second process");
    //    info!("Main task waiting for process id 1 to complete");
    //    spawn_proc.wait().expect("Unable to wait on process id 1");
    //    
    //    let spawn_task = sched::create_thread(task_spawn).unwrap();

    //    info!("Main task waiting for task id 1 to complete");
    //    spawn_task.wait().expect("Unable to wait on task id 1");
    //}

    //{
    //    let user_proc0 = sched::create_process(|| -> ! {loop{}}, true)
    //    .expect("Failed to create user process 0");
    //    
    //    sched::create_thread(watchdog).unwrap();
    //}

    //sched::create_thread(thread_creator).expect("Failed to create kernel thread!");

    //sched::create_thread(|| {
    //    loop {
    //        {
    //            let mut queue = QUEUE.lock();
    //            queue.add_node([0; 64]).expect("Failed to add node from producer!");
    //            let addr = queue.last().unwrap().as_ptr().addr();
    //            debug!("Added new node at address {:#X}", addr);
    //        }
    //        sched::delay_ms(1000);
    //    }
    //}).expect("Failed to create producer thread!");

    //sched::create_process(|| {
    //   loop {
    //        {
    //            let mut queue = QUEUE.lock();
    //            let node = queue.first();
    //            if node.is_some() {
    //                let node = node.unwrap();
    //                debug!("[Consumer]: Found node at address: {:#X}", node.as_ptr().addr());
    //            }
    //            queue.pop_node();
    //        }
    //        sched::delay_ms(1000);
    //   } 
    //}, false).expect("Failed to create consumer process!");


    loop {
        sched::delay_ms(1000);
        info!("Main task looping");
    }

    hal::sleep();
}

// This will be called from the entry point for the corresponding arch
// For now, we support only x86_64, so the entry point is at kernel/src/hal/x86_64/asm/kernel_entry_stub.S
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

static KEYBOARD_EVENT: Once<KSem> = Once::new();

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

fn watchdog() -> ! {
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