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
mod utils;

#[cfg(feature = "acpi")]
mod acpica;

use core::sync::atomic::{AtomicBool, Ordering};
use core::sync::atomic::AtomicUsize;
use core::ffi::c_void;
use io::DeviceHandleK;

use kernel_intf::{info, debug};
use common::*;
use loader::module;

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::vec;

#[cfg(test)]
mod tests;

use sync::{Once, Spinlock};
use mem::Regions::*;
use mem::FixedList;
use kernel_intf::driver::{IrpMajor, IrpMinor, IrpResult, Status, create_device_by_id};
use kernel_intf::KError;
use kernel_intf::list::List;
use sync::KSem;

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
static THREAD_DONE_SEM: Once<KSem> = Once::new();


// Simple worker used in run_proc_thread_tests test 3.
extern "C" fn test_thread_runner() -> ! {
    let id = sched::get_current_task_id().unwrap_or(0);
    info!("test_thread_runner: started (id={})", id);
    sched::delay_ms(500);
    info!("test_thread_runner: signaling done (id={})", id);
    THREAD_DONE_SEM.get().unwrap().signal();
    sched::exit_thread(0);
}

// Tests:
//   1. Single kernel process create + wait + exit-code check.
//   2. Three concurrent kernel processes, each waited individually.
//   3. Three worker threads synchronised through a semaphore.
fn run_proc_thread_tests() {
    info!("=== run_proc_thread_tests: BEGIN ===");

    // Test 1: single process with context_ptr; verify the module mutates it
    info!("--- proc/thread test 1: single process with context ---");

    #[repr(C)]
    struct TestCtx {
        val1: usize,
        val2: isize,
    }

    let mut ctx = TestCtx { val1: 42, val2: -7 };
    let proc1 = sched::create_process(
        vec!["libtest1.so".into(), "hello_from_test1".into()],
        &mut ctx as *mut TestCtx as *mut c_void,
        false,
    )
    .expect("proc/thread test 1: failed to create process");

    proc1.wait().expect("proc/thread test 1: wait failed");
    let code1 = proc1.lock().get_exit_code();
    info!("proc/thread test 1: process exited with code {}", code1);
    info!("proc/thread test 1: ctx.val1 after = {} (expect 10)", ctx.val1);

    // Test 2: three concurrent processes (no context), wait for all
    info!("--- proc/thread test 2: concurrent processes ---");

    const PROC_COUNT: usize = 3;
    let mut procs = alloc::vec::Vec::with_capacity(PROC_COUNT);
    for i in 0..PROC_COUNT {
        let p = sched::create_process(
            vec!["libtest1.so".into(), alloc::format!("concurrent_proc_{}", i)],
            core::ptr::null_mut(),
            false,
        )
        .expect("proc/thread test 2: failed to create process");
        info!("proc/thread test 2: launched process {} (id={})", i, p.lock().get_id());
        procs.push(p);
    }

    for (i, p) in procs.iter().enumerate() {
        p.wait().expect("proc/thread test 2: wait failed");
        info!("proc/thread test 2: process {} exited with code {}", i, p.lock().get_exit_code());
    }

    // Test 3: thread creation + semaphore synchronisation
    info!("--- proc/thread test 3: thread creation ---");

    const THREAD_COUNT: usize = 3;
    THREAD_DONE_SEM.call_once(|| KSem::new(0, THREAD_COUNT as isize));

    let mut threads = alloc::vec::Vec::with_capacity(THREAD_COUNT);
    for _ in 0..THREAD_COUNT {
        threads.push(
            sched::create_thread(test_thread_runner, core::ptr::null_mut())
                .expect("proc/thread test 3: failed to create thread"),
        );
    }

    // Wait for every thread to signal it has finished.
    for _ in 0..THREAD_COUNT {
        THREAD_DONE_SEM.get().unwrap().wait().unwrap();
    }
    info!("proc/thread test 3: all {} threads completed", THREAD_COUNT);
    drop(threads);

    info!("=== run_proc_thread_tests: PASSED ===");
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

extern "C" fn task_kill_cancel_runner() -> ! {
    let handle = match io::open_device_handle("test1") {
        Ok(h) => h,
        Err(_) => {
            info!("task_kill_cancel_runner: test1 device not found");
            sched::exit_thread(0);
        }
    };

    info!("task_kill_cancel_runner: issuing sync read");
    let _ = handle.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: 0, size: 0 },
        offset: 0,
    });

    // If kill_thread fired during the wait, kill_sweep_irps cancelled the
    // IRP, the driver's cancel routine completed it, and the event was
    // signalled with status cancelled. The thread won't make it here in
    // practice (it's already terminated).
    info!("task_kill_cancel_runner: read returned (post-cancel)");
    loop { sched::delay_ms(1000); }
}

extern "C" fn self_cancel_runner() -> ! {
    let handle = match io::open_device_handle("test1") {
        Ok(h) => h,
        Err(_) => {
            info!("self_cancel_runner: test1 device not found");
            sched::exit_thread(0);
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
    sched::exit_thread(0);
}

fn run_cancel_tests() {
    // (a) task-kill cancellation
    let killer_target = sched::create_thread(task_kill_cancel_runner, core::ptr::null_mut())
        .expect("Failed to spawn task_kill_cancel_runner");
    sched::delay_ms(300);
    let tid = killer_target.lock().get_id();
    info!("cancel test (a): killing thread {}", tid);
    sched::kill_thread(tid, 0);
    // Wait long enough that the driver worker hits io_start_processing and
    // exits cleanly (1500ms sleep in test1::read_worker + slack).
    sched::delay_ms(2500);

    // (b) per-handle self-cancellation
    let _ = sched::create_thread(self_cancel_runner, core::ptr::null_mut())
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

static STATE_TEST_DEV: Once<DeviceHandleK> = Once::new();
static STATE_TEST_RUN: AtomicBool = AtomicBool::new(false);
static REJECTED_DURING_STOPPED: AtomicUsize = AtomicUsize::new(0);
static SUCCESS_AFTER_START: AtomicUsize = AtomicUsize::new(0);
static REJECTED_AFTER_REMOVE: AtomicUsize = AtomicUsize::new(0);

fn state_test_handle() -> DeviceHandleK {
    STATE_TEST_DEV.get().expect("state test device handle not initialised").clone()
}

// Bash on the device with reads while STATE_TEST_RUN; tally rejections.
extern "C" fn state_reject_loop() -> ! {
    let dev = state_test_handle();
    while STATE_TEST_RUN.load(Ordering::Acquire) {
        let res = dev.read(io::ReadRequest {
            buffer: MemoryRegion { base_address: 0, size: 0 },
            offset: 0,
        });
        if let Err(KError::DeviceStopped) = res {
            REJECTED_DURING_STOPPED.fetch_add(1, Ordering::Relaxed);
        }
        sched::delay_ms(10);
    }
    sched::exit_thread(0);
}

extern "C" fn state_start_once() -> ! {
    let dev = state_test_handle();
    let r = dev.start();
    info!("state_start_once (tid={}): start -> {:?}",
          sched::get_current_task_id().unwrap_or(0),
          r.map(|s| s as isize));
    sched::exit_thread(0);
}

extern "C" fn state_write_once() -> ! {
    let dev = state_test_handle();
    let res = dev.write(io::ReadRequest {
        buffer: MemoryRegion { base_address: 0, size: 0 },
        offset: 0,
    });
    if let Ok(Status::Success) = res {
        SUCCESS_AFTER_START.fetch_add(1, Ordering::Relaxed);
    }
    info!("state_write_once (tid={}): write -> {:?}",
          sched::get_current_task_id().unwrap_or(0),
          res.map(|s| s as isize));
    sched::exit_thread(0);
}

extern "C" fn state_post_remove_read() -> ! {
    let dev = state_test_handle();
    let res = dev.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: 0, size: 0 },
        offset: 0,
    });
    if let Err(KError::DeviceStopped) = res {
        REJECTED_AFTER_REMOVE.fetch_add(1, Ordering::Relaxed);
    }
    info!("state_post_remove_read: read -> {:?}", res.map(|s| s as isize));
    sched::exit_thread(0);
}

fn run_state_tests() {
    info!("=== run_state_tests: BEGIN ===");

    let dev = match io::open_device_handle("test1") {
        Ok(h) => h,
        Err(_) => {
            info!("run_state_tests: SKIP — test1 device missing");
            return;
        }
    };
    STATE_TEST_DEV.call_once(|| dev.clone());
    let dev_id = dev.id();

    // Phase 1 — Started baseline.
    let baseline = dev.write(io::ReadRequest {
        buffer: MemoryRegion { base_address: 0, size: 0 },
        offset: 0,
    });
    info!("state phase 1 (Started baseline): write -> {:?}", baseline.map(|s| s as isize));
    assert!(matches!(baseline, Ok(Status::Success)),
            "baseline write on Started device must succeed");

    // Phase 2 — Stop + concurrent rejected I/O.
    STATE_TEST_RUN.store(true, Ordering::Release);
    REJECTED_DURING_STOPPED.store(0, Ordering::Relaxed);
    for _ in 0..3 {
        sched::create_thread(state_reject_loop, core::ptr::null_mut()).expect("Failed to spawn state_reject_loop");
    }
    sched::delay_ms(50);                // let the readers start spinning
    info!("state phase 2: stopping device");
    dev.stop().expect("stop must succeed");
    sched::delay_ms(300);
    STATE_TEST_RUN.store(false, Ordering::Release);
    sched::delay_ms(100);               // let the reject loops exit
    let rejected = REJECTED_DURING_STOPPED.load(Ordering::Relaxed);
    info!("state phase 2: rejected_during_stopped = {}", rejected);
    assert!(rejected > 0, "reads issued after stop must be rejected");

    // Phase 3 — Concurrent starts. Three threads attempt Start; serialized by
    // config_guard, exactly one wins (state == Stopped); the others see Started
    // and bail with DeviceStopped.
    for _ in 0..3 {
        sched::create_thread(state_start_once, core::ptr::null_mut()).expect("Failed to spawn state_start_once");
    }
    sched::delay_ms(300);
    let post_start_write = dev.write(io::ReadRequest {
        buffer: MemoryRegion { base_address: 0, size: 0 },
        offset: 0,
    });
    info!("state phase 3: post-start sanity write -> {:?}", post_start_write.map(|s| s as isize));
    assert!(matches!(post_start_write, Ok(Status::Success)),
            "device must be Started after concurrent start attempts");

    // Phase 4 — Concurrent writes from multiple threads (state must pass them
    // all through the Started guard).
    SUCCESS_AFTER_START.store(0, Ordering::Relaxed);
    for _ in 0..3 {
        sched::create_thread(state_write_once, core::ptr::null_mut()).expect("Failed to spawn state_write_once");
    }
    sched::delay_ms(300);
    let succ = SUCCESS_AFTER_START.load(Ordering::Relaxed);
    info!("state phase 4: success_after_start = {}", succ);
    assert!(succ > 0, "at least one concurrent write after restart must succeed");

    // Phase 5 — Remove + post-remove rejection.
    info!("state phase 5: removing device {}", dev_id);
    io::remove_device_async(dev_id);
    io::pnp_fence();                    // wait until the worker finishes the removal
    REJECTED_AFTER_REMOVE.store(0, Ordering::Relaxed);
    for _ in 0..3 {
        sched::create_thread(state_post_remove_read, core::ptr::null_mut()).expect("Failed to spawn state_post_remove_read");
    }
    sched::delay_ms(300);
    let post_rm = REJECTED_AFTER_REMOVE.load(Ordering::Relaxed);
    info!("state phase 5: rejected_after_remove = {}", post_rm);
    assert!(post_rm == 3, "every read on a Removed device must be rejected");

    let post_remove_start = dev.start();
    info!("state phase 5: post-remove start -> {:?}", post_remove_start.map(|s| s as isize));
    assert!(matches!(post_remove_start, Err(KError::DeviceStopped)),
            "Removed device must not be restartable");

    info!("=== run_state_tests: PASSED ===");
}

// === PnP fence test ===
fn run_fence_tests() {
    info!("=== run_fence_tests: BEGIN ===");

    // Batch 1: post 4 register_driver calls, then fence.
    for _ in 0..4 {
        io::register_driver("test1".into());
    }
    info!("fence test: posted batch 1 (4x register_driver test1), entering fence");
    io::pnp_fence();
    info!("fence test: batch 1 fence returned");

    // Batch 2: post 3 more, then fence.
    for _ in 0..3 {
        io::register_driver("test2".into());
    }
    info!("fence test: posted batch 2 (3x register_driver test2), entering fence");
    io::pnp_fence();
    info!("fence test: batch 2 fence returned");

    info!("=== run_fence_tests: PASSED ===");
}

extern "C" fn thread_creator() -> ! {
    info!("Created new thread");
    loop {
        sched::create_thread(thread_creator, core::ptr::null_mut()).expect("Failed to create kernel thread!");
        sched::delay_ms(500);
    }
}

fn spam_threads_test() {
    sched::create_thread(thread_creator, core::ptr::null_mut()).expect("Failed to create kernel thread!");
}

fn kern_main() -> ! {
    info!("Starting main kernel init");

    sched::init();
    loader::init();
    io::init();

    //interative_thread_spawn_tests();
    //run_fence_tests();
    //run_state_tests();
    //run_i8042_tests();

    //run_proc_thread_tests();
    spam_threads_test();
    info!("Main task going to sleep");
    info!("====TTY mode====");
    hal::sleep();
}

// The driver satisfies read IRPs only when the requested number of characters
// has accumulated in the interrupt-driven ring buffer.  These tests therefore
// block until the user presses enough keys on the physical (or QEMU) keyboard.
//
// Test layout:
//   1. Single-threaded read: request 3 characters (main thread blocks).
//   2. Three concurrent readers requesting 2, 4, and 1 character respectively.
//      All are queued as pending IRPs; the ISR satisfies them as keys arrive.

fn run_i8042_tests() {
    // Give the PnP worker time to bring the i8042 device to Started state.
    sched::delay_ms(500);

    let handle = match io::open_device_handle("ps/2_port0") {
        Ok(h) => h,
        Err(_) => {
            info!("i8042 test: device ps/2_port0 not found, skipping");
            return;
        }
    };

    // Test 1: single reader, 3 characters 
    info!("i8042 test 1: press 3 keys to satisfy single read...");
    let mut buf1 = [0u8; 3];
    let _ = handle.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: buf1.as_mut_ptr() as usize, size: 3 },
        offset: 0,
    });
    info!("i8042 test 1: got chars: {:?}", core::str::from_utf8(&buf1).unwrap_or("?"));

    // Test 2: three concurrent readers 
    // Each reader opens its own handle and issues a read of a specific length.
    info!("i8042 test 2: spawning 3 concurrent readers (need 2+4+1 = 7 more keys)...");
    sched::create_thread(i8042_reader_a, core::ptr::null_mut()).expect("i8042: spawn reader-a");
    sched::create_thread(i8042_reader_b, core::ptr::null_mut()).expect("i8042: spawn reader-b");
    sched::create_thread(i8042_reader_c, core::ptr::null_mut()).expect("i8042: spawn reader-c");
}

extern "C" fn i8042_reader_a() -> ! {
    let h = io::open_device_handle("ps/2_port0").expect("i8042 reader-a: no device");
    let mut buf = [0u8; 2];
    info!("i8042 reader-a: waiting for 2 chars");
    let _ = h.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: buf.as_mut_ptr() as usize, size: 2 },
        offset: 0,
    });
    info!("i8042 reader-a: got chars: {:?}", core::str::from_utf8(&buf).unwrap_or("?"));
    sched::exit_thread(0)
}

extern "C" fn i8042_reader_b() -> ! {
    let h = io::open_device_handle("ps/2_port0").expect("i8042 reader-b: no device");
    let mut buf = [0u8; 4];
    info!("i8042 reader-b: waiting for 4 chars");
    let _ = h.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: buf.as_mut_ptr() as usize, size: 4 },
        offset: 0,
    });
    info!("i8042 reader-b: got chars: {:?}", core::str::from_utf8(&buf).unwrap_or("?"));
    sched::exit_thread(0)
}

extern "C" fn i8042_reader_c() -> ! {
    let h = io::open_device_handle("ps/2_port0").expect("i8042 reader-c: no device");
    let mut buf = [0u8; 1];
    info!("i8042 reader-c: waiting for 1 char");
    let _ = h.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: buf.as_mut_ptr() as usize, size: 1 },
        offset: 0,
    });
    info!("i8042 reader-c: got char: {:?}", core::str::from_utf8(&buf).unwrap_or("?"));
    sched::exit_thread(0)
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