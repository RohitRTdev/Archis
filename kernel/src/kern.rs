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
mod object;
mod utils;
mod pipe;

#[cfg(feature = "acpi")]
mod acpica;

use crate::io::{open_device_handle, pnp_post};

#[cfg(feature = "kunit-test")] 
use {
    core::sync::atomic::{AtomicUsize, AtomicBool, Ordering},
    core::ffi::c_void,
    io::OpenDeviceHandle
};

use kernel_intf::{info, debug, ExitInfo};
use common::*;
use loader::module;

extern crate alloc;
use alloc::collections::BTreeMap;
#[cfg(feature = "kunit-test")]
use alloc::vec;

#[cfg(test)]
mod tests;

use sync::{Once, Spinlock};
use mem::Regions::*;
use mem::FixedList;
#[cfg(feature = "kunit-test")]
use kernel_intf::driver::{DeviceType, IrpMajor, IrpMinor, IrpResult, Keystroke, Status, create_device_by_id};
#[cfg(feature = "kunit-test")]
use kernel_intf::KError;
use kernel_intf::list::List;
#[cfg(feature = "kunit-test")]
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

// "9ffd2959-915c-479f-8787-1f9f701e1034"
pub const ROOT_UUID: [u8; 16] = [
    0x9f, 0xfd, 0x29, 0x59, 0x91, 0x5c, 0x47, 0x9f,
    0x87, 0x87, 0x1f, 0x9f, 0x70, 0x1e, 0x10, 0x34
];

struct InitFS {
    fs: BTreeMap<&'static str, &'static [u8]>,
    symlinks: BTreeMap<&'static str, &'static str>
}

static INIT_FS: Once<InitFS> = Once::new();  
static REMAP_LIST: Spinlock<FixedList<RemapEntry, {Region2 as usize}>> = Spinlock::new(List::new());
#[cfg(feature = "kunit-test")]
static THREAD_DONE_SEM: Once<crate::sync::KSem> = Once::new();

// Simple worker used in run_proc_thread_tests test 3.
#[cfg(feature = "kunit-test")]
extern "C" fn test_thread_runner() -> ! {
    let id = sched::get_current_task_id().unwrap_or(0);
    info!("test_thread_runner: started (id={})", id);
    sched::delay_ms(500, false);
    info!("test_thread_runner: signaling done (id={})", id);
    THREAD_DONE_SEM.get().unwrap().signal();
    sched::exit_thread(ExitInfo::normal(0));
}

// Tests:
//   1. Single kernel process create + wait + exit-code check.
//   2. Three concurrent kernel processes, each waited individually.
//   3. Three worker threads synchronised through a semaphore.
#[kmod::test_function(false)]
fn run_proc_thread_tests() {
    // Test 1: single process with context_ptr; verify the module mutates it
    info!("--- proc/thread test 1: single process with context ---");

    #[repr(C)]
    struct TestCtx {
        val1: usize,
        val2: isize,
    }

    let mut ctx = TestCtx { val1: 42, val2: -7 };
    let proc1 = sched::create_process(
        &["libtest1.so", "hello_from_test1"],
        &[],
        &mut ctx as *mut TestCtx as *mut core::ffi::c_void,
        false,
        false
    )
    .expect("proc/thread test 1: failed to create process");

    proc1.wait(false);
    let code1 = proc1.lock().get_exit_info().code;
    info!("proc/thread test 1: process exited with code {}", code1);
    info!("proc/thread test 1: ctx.val1 after = {} (expect 10)", ctx.val1);

    // Test 2: three concurrent processes (no context), wait for all
    info!("--- proc/thread test 2: concurrent processes ---");

    const PROC_COUNT: usize = 3;
    let mut procs = alloc::vec::Vec::with_capacity(PROC_COUNT);
    for i in 0..PROC_COUNT {
        let p = sched::create_process(
            &["libtest1.so", alloc::format!("concurrent_proc_{}", i).as_str()],
            &[],
            core::ptr::null_mut(),
            false,
            false
        )
        .expect("proc/thread test 2: failed to create process");
        info!("proc/thread test 2: launched process {} (id={})", i, p.lock().get_id());
        procs.push(p);
    }

    for (i, p) in procs.iter().enumerate() {
        p.wait(false);
        info!("proc/thread test 2: process {} exited with code {}", i, p.lock().get_exit_info().code);
    }

    // Test 3: thread creation + semaphore synchronisation
    info!("--- proc/thread test 3: thread creation ---");

    const THREAD_COUNT: usize = 3;
    THREAD_DONE_SEM.call_once(|| crate::sync::KSem::new(0, THREAD_COUNT as isize));

    let mut threads = alloc::vec::Vec::with_capacity(THREAD_COUNT);
    for _ in 0..THREAD_COUNT {
        threads.push(
            sched::create_thread(test_thread_runner, core::ptr::null_mut())
                .expect("proc/thread test 3: failed to create thread"),
        );
    }

    // Wait for every thread to signal it has finished.
    for _ in 0..THREAD_COUNT {
        THREAD_DONE_SEM.get().unwrap().wait(false);
    }
    info!("proc/thread test 3: all {} threads completed", THREAD_COUNT);
    drop(threads);

    info!("=== run_proc_thread_tests: PASSED ===");
}

// === IRP cancellation tests ===
//   (a) task-kill cancellation — kill_thread → kill_sweep_irps walks the
//       dying thread's IRP list and invokes the driver's cancel routine.
//   (b) per-handle cancellation — issuing thread itself calls
//       cancel_pending_irp on the device handle.
//   (c) duplicate device name guard — io_create_device must return null
//       when the name is already in DEVICE_BY_NAME.
//   (d) OpenDeviceHandle RAII — open sends Open IRP; drop sends Close IRP.
//
// All tests use the "input" class device (started by the input driver once
// i8042 is up). A read for 1 char pends until a key is pressed, making it
// an ideal subject for cancellation.

#[cfg(feature = "kunit-test")]
extern "C" fn cancel_test_completion(result: *const IrpResult, _ctx: *mut core::ffi::c_void) {
    let status = unsafe { (*result).status };
    info!("cancel_test_completion: IRP delivered with status {}", status as isize);
}

#[cfg(feature = "kunit-test")]
extern "C" fn task_kill_cancel_runner() -> ! {
    let handle = match io::open_device_handle("input") {
        Ok(h) => h,
        Err(_) => {
            info!("task_kill_cancel_runner: input device could not be opened");
            sched::exit_thread(ExitInfo::normal(0));
        }
    };

    let mut buf = [0u8; 1];
    info!("task_kill_cancel_runner: issuing sync read (will pend until killed)");
    let res= handle.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: buf.as_mut_ptr() as usize, size: 1 },
        offset: 0
    }, false);

    info!("task_kill_cancel_runner: read returned (post-cancel) with {:?}", res);
    loop { sched::delay_ms(1000, false); }
}

#[cfg(feature = "kunit-test")]
extern "C" fn self_cancel_runner() -> ! {
    let handle = match io::open_device_handle("input") {
        Ok(h) => h,
        Err(_) => {
            info!("self_cancel_runner: input device could not be opened");
            sched::exit_thread(ExitInfo::normal(0));
        }
    };

    let mut buf = [0u8; 1];
    info!("self_cancel_runner: dispatching async read");
    let res = io::io_request_async(
        &handle,
        IrpMajor::Read,
        IrpMinor::None,
        MemoryRegion { base_address: buf.as_mut_ptr() as usize, size: 1 },
        0,
        None,
        cancel_test_completion,
        core::ptr::null_mut(),
    );

    sched::delay_ms(200, false);
    info!("self_cancel_runner: calling cancel_pending_irp with {:?}", res);
    io::cancel_pending_irp(&handle);

    sched::delay_ms(200, false);
    info!(
        "self_cancel_runner: pending_irps on input = {}",
        handle.get_pending_irps().lock().get_nodes()
    );
    sched::exit_thread(ExitInfo::normal(0));
}

#[kmod::test_function(false)]
fn run_cancel_tests() {
    // Wait for PnP to start i8042 and the input class device.
    sched::delay_ms(500, false);

    // task-kill cancellation
    let killer_target = sched::create_thread(task_kill_cancel_runner, core::ptr::null_mut())
        .expect("Failed to spawn task_kill_cancel_runner");
    sched::delay_ms(2000, false);
    let tid = killer_target.lock().get_id();
    info!("cancel test (a): killing thread {}", tid);
    sched::kill_thread(tid, ExitInfo::normal(0));
    sched::delay_ms(500, false);

    // per-handle self-cancellation
    let _ = sched::create_thread(self_cancel_runner, core::ptr::null_mut())
        .expect("Failed to spawn self_cancel_runner");
    sched::delay_ms(1000, false);

    // duplicate device name guard.
    let probe = match io::open_device_handle("input") {
        Ok(h) => h,
        Err(_) => {
            info!("cancel test (c): SKIP — input device missing");
            return;
        }
    };
    let driver_id = unsafe { (*probe.device_ptr()).get_driver_id() };
    let dup = create_device_by_id(driver_id, Some("input"), core::ptr::null_mut(), None, false, DeviceType::Input);
    info!("cancel test (c): duplicate create returned {:#X}", dup.addr());
    assert!(dup.is_null(), "duplicate device name 'input' must be rejected");
    info!("cancel test (c): PASSED");

    // OpenDeviceHandle Open/Close IRP lifecycle.
    info!("cancel test (d): OpenDeviceHandle Open/Close lifecycle");
    match io::open_device_handle("input") {
        Ok(h) => {
            info!("cancel test (d): opened 'input' — Open IRP sent");
            drop(h);
            info!("cancel test (d): handle dropped — Close IRP sent");
            info!("cancel test (d): PASSED");
        }
        Err(e) => {
            info!("cancel test (d): SKIP — 'input' not available: {}", e);
        }
    }
}

#[cfg(feature = "kunit-test")]
static STATE_TEST_DEV: Once<OpenDeviceHandle> = Once::new();
#[cfg(feature = "kunit-test")]
static STATE_TEST_RUN: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "kunit-test")]
static REJECTED_DURING_STOPPED: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "kunit-test")]
static REACHED_DRIVER_AFTER_START: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "kunit-test")]
fn state_test_handle() -> OpenDeviceHandle {
    STATE_TEST_DEV.get().expect("state test device handle not initialised").clone()
}

// Spin-read with size 0 while STATE_TEST_RUN; tally DeviceStopped rejections.
// i8042 rejects size-0 reads with Status::Failed when started (reaches driver),
// so this never pends — the loop spins freely and only counts stopped-state hits.
#[cfg(feature = "kunit-test")]
extern "C" fn state_reject_loop() -> ! {
    let dev = state_test_handle();
    while STATE_TEST_RUN.load(Ordering::Acquire) {
        let res = dev.read(io::ReadRequest {
            buffer: MemoryRegion { base_address: 0, size: 0 },
            offset: 0
        }, false);
        if let Err(KError::DeviceStopped) = res {
            REJECTED_DURING_STOPPED.fetch_add(1, Ordering::Relaxed);
        }
        sched::delay_ms(10, false);
    }
    sched::exit_thread(ExitInfo::normal(0));
}

#[cfg(feature = "kunit-test")]
extern "C" fn state_start_once() -> ! {
    let dev = state_test_handle();
    let r = dev.start();
    info!("state_start_once (tid={}): start -> {:?}",
          sched::get_current_task_id().unwrap_or(0),
          r.map(|s| s as isize));
    sched::exit_thread(ExitInfo::normal(0));
}

// Issue a size-0 read. i8042 returns Ok(Status::Failed) for empty buffers when
// started (request reached the driver). Any Err variant means the state guard
// rejected it before dispatch.
#[cfg(feature = "kunit-test")]
extern "C" fn state_io_once() -> ! {
    let dev = state_test_handle();
    let res = dev.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: 0, size: 0 },
        offset: 0
    }, false);
    if matches!(res, Ok(_)) {
        REACHED_DRIVER_AFTER_START.fetch_add(1, Ordering::Relaxed);
    }
    info!("state_io_once (tid={}): read -> {:?}",
          sched::get_current_task_id().unwrap_or(0),
          res.map(|s| s as isize));
    sched::exit_thread(ExitInfo::normal(0));
}

#[kmod::test_function(false)]
fn run_state_tests() {
    // Wait for PnP to bring up i8042.
    sched::delay_ms(500, false);

    let dev = match io::open_device_handle("ps/2_port0") {
        Ok(h) => h,
        Err(_) => {
            info!("run_state_tests: SKIP — ps/2_port0 device missing");
            return;
        }
    };
    STATE_TEST_DEV.call_once(|| dev.clone());

    // Phase 1 — Started baseline: size-0 read must reach the driver (Ok(_)),
    // not be turned away by the state guard (Err(_)).
    let baseline = dev.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: 0, size: 0 },
        offset: 0
    }, false);
    info!("state phase 1 (Started baseline): read -> {:?}", baseline.map(|s| s as isize));
    assert!(matches!(baseline, Ok(_)), "baseline read on Started device must reach the driver");

    // Phase 2 — Stop + concurrent rejected I/O.
    STATE_TEST_RUN.store(true, Ordering::Release);
    REJECTED_DURING_STOPPED.store(0, Ordering::Relaxed);
    for _ in 0..3 {
        sched::create_thread(state_reject_loop, core::ptr::null_mut()).expect("Failed to spawn state_reject_loop");
    }
    sched::delay_ms(50, false);
    info!("state phase 2: stopping device");
    dev.stop(false).expect("stop must succeed");
    sched::delay_ms(300, false);
    STATE_TEST_RUN.store(false, Ordering::Release);
    sched::delay_ms(100, false);
    let rejected = REJECTED_DURING_STOPPED.load(Ordering::Relaxed);
    info!("state phase 2: rejected_during_stopped = {}", rejected);
    assert!(rejected > 0, "reads issued after stop must be rejected");

    // Phase 3 — Concurrent starts. Three threads attempt Start; serialized by
    // config_guard, exactly one wins (state == Stopped); the others get
    // DeviceStarted (already Started).
    for _ in 0..3 {
        sched::create_thread(state_start_once, core::ptr::null_mut()).expect("Failed to spawn state_start_once");
    }
    sched::delay_ms(300, false);
    let post_start = dev.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: 0, size: 0 },
        offset: 0
    }, false);
    info!("state phase 3: post-start sanity read -> {:?}", post_start.map(|s| s as isize));
    assert!(matches!(post_start, Ok(_)),
            "device must be Started after concurrent start attempts");

    // Phase 4 — Concurrent reads from multiple threads must all pass through
    // the Started state guard and reach the driver.
    REACHED_DRIVER_AFTER_START.store(0, Ordering::Relaxed);
    for _ in 0..3 {
        sched::create_thread(state_io_once, core::ptr::null_mut()).expect("Failed to spawn state_io_once");
    }
    sched::delay_ms(300, false);
    let reached = REACHED_DRIVER_AFTER_START.load(Ordering::Relaxed);
    info!("state phase 4: reached_driver_after_start = {}", reached);
    assert!(reached > 0, "concurrent reads on Started device must reach the driver");

    info!("=== run_state_tests: PASSED ===");
}

// === PnP fence test ===
#[kmod::test_function(false)]
fn run_fence_tests() {
    // Batch 1: post 4 register_driver calls, then fence.
    // Already-loaded drivers are handled gracefully by the PnP worker; what
    // matters is that pnp_fence() blocks until all queued work is drained.
    for _ in 0..4 {
        io::add_config("i8042".into());
    }
    info!("fence test: posted batch 1 (4x register_driver i8042), entering fence");
    io::pnp_fence();
    info!("fence test: batch 1 fence returned");

    // Batch 2: post 3 more, then fence.
    for _ in 0..3 {
        io::add_config("input".into());
    }
    info!("fence test: posted batch 2 (3x register_driver input), entering fence");
    io::pnp_fence();
    info!("fence test: batch 2 fence returned");

    info!("=== run_fence_tests: PASSED ===");
}

#[cfg(feature = "kunit-test")]
extern "C" fn thread_creator() -> ! {
    info!("Created new thread");
    loop {
        sched::create_thread(thread_creator, core::ptr::null_mut()).expect("Failed to create kernel thread!");
        sched::delay_ms(500, false);
    }
}

#[kmod::test_function(false)]
fn spam_threads_test() {
    sched::create_thread(thread_creator, core::ptr::null_mut()).expect("Failed to create kernel thread!");
}

// === KSem / KEvent test suite ===
//
//   - try_wait (timeout=0) must not mutate the semaphore counter.
//   - last_wait_on_expired_timer must be reset on every wait (otherwise a
//     stale TRUE from a previous timed-out wait leaks into the next call).
//   - timer cleanup on signal must match by (task_id, sem), not just sem.
//     Otherwise task A being signalled wipes task B's timer.
//   - timer-fired-after-signal race must not double-bump the counter.

#[cfg(feature = "kunit-test")]
mod test_sync {
    use super::*;

    pub static SYNC_TEST_SEM: Once<KSem> = Once::new();
    pub static SYNC_TEST_DONE: Once<KSem> = Once::new();
    pub static SYNC_AUTO_EVENT: Once<KEvent> = Once::new();
    pub static SYNC_MANUAL_EVENT: Once<KEvent> = Once::new();

    pub static SYNC_WAKE_COUNT: AtomicUsize = AtomicUsize::new(0);
    pub static SYNC_TIMEOUT_COUNT: AtomicUsize = AtomicUsize::new(0);
    pub static SYNC_SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);
}

#[cfg(feature = "kunit-test")]
use test_sync::*;

#[cfg(feature = "kunit-test")]
fn reset_sync_counters() {
    SYNC_WAKE_COUNT.store(0, Ordering::Relaxed);
    SYNC_TIMEOUT_COUNT.store(0, Ordering::Relaxed);
    SYNC_SIGNAL_COUNT.store(0, Ordering::Relaxed);
}

#[cfg(feature = "kunit-test")]
fn join_workers(done_sem: &KSem, count: usize) {
    for _ in 0..count {
        if done_sem.wait_with_timeout(10000, false).is_err() {
            debug!("join workers exited due to timeout!");
        }
    }
}

#[kmod::test_function(false)]
fn run_sync_tests() {
    // Initialise the global slots used by all sub-tests. We re-create the
    // KSem/KEvent inside each test that needs different state, but the Once
    // is filled with a placeholder first so .get() never panics.
    SYNC_TEST_SEM.call_once(|| KSem::new(0, 1));
    SYNC_TEST_DONE.call_once(|| KSem::new(0, 200));
    SYNC_AUTO_EVENT.call_once(|| KEvent::new(true));
    SYNC_MANUAL_EVENT.call_once(|| KEvent::new(false));

    // Test 1: counting semaphore basic — signal N, wait N (no blocking)
    info!("sync test 1: counting semaphore basic");
    {
        let sem = KSem::new(0, 4);
        sem.signal();
        sem.signal();
        sem.signal();
        // Three signals queued; three try_wait should succeed.
        assert!(sem.wait_with_timeout(0, false).is_ok(), "test 1: first try_wait expected success");
        assert!(sem.wait_with_timeout(0, false).is_ok(), "test 1: second try_wait expected success");
        assert!(sem.wait_with_timeout(0, false).is_ok(), "test 1: third try_wait expected success");
        // Fourth must fail (counter back at 0).
        assert!(!sem.wait_with_timeout(0, false).is_ok(), "test 1: fourth try_wait must fail");
        // Confirm counter wasn't corrupted by the failed try_wait.
        sem.signal();
        assert!(sem.wait_with_timeout(0, false).is_ok(),
            "test 1: after signal post-failed-try_wait, try_wait must succeed (regression for try_wait counter leak)");
    }
    info!("sync test 1: PASSED");

    // Test 2: counting semaphore max_count clamp
    info!("sync test 2: counter clamped to max_count");
    {
        let sem = KSem::new(2, 2);   // already at max
        sem.signal();                // must clamp, not overflow
        sem.signal();
        assert!(sem.wait_with_timeout(0, false).is_ok(), "test 2: 1st try_wait");
        assert!(sem.wait_with_timeout(0, false).is_ok(), "test 2: 2nd try_wait");
        assert!(!sem.wait_with_timeout(0, false).is_ok(), "test 2: 3rd try_wait must fail (counter must have been clamped at 2)");
    }
    info!("sync test 2: PASSED");

    // Test 3: timeout actually expires; flag flips correctly
    info!("sync test 3: wait_with_timeout expires without signal");
    {
        let sem = KSem::new(0, 1);
        let res = sem.wait_with_timeout(200, false).is_ok();
        assert!(!res, "test 3: timed wait must return false on expiry");
    }
    info!("sync test 3: PASSED");

    // Test 4: stale-flag regression — wait that doesn't block after one
    // that timed out must NOT return "timeout".
    info!("sync test 4: stale last_wait_on_expired_timer must be reset");
    {
        let sem = KSem::new(0, 1);
        let res1 = sem.wait_with_timeout(100, false).is_ok();
        assert!(!res1, "test 4: setup wait must time out");
        // Now make the next wait succeed immediately.
        sem.signal();
        let res2 = sem.wait(false).is_ok();
        assert!(res2, "test 4: immediate-success wait returned false — stale timer flag (regression)");
    }
    info!("sync test 4: PASSED");

    // Test 5: signal arrives before timeout — returns true
    info!("sync test 5: signal arrives before timeout");
    {
        let sem = KSem::new(0, 1);
        // Spawn a thread that signals after 200ms.
        static T5_SEM: Once<KSem> = Once::new();
        T5_SEM.call_once(|| sem.clone());

        extern "C" fn t5_signaller() -> ! {
            sched::delay_ms(200, false);
            T5_SEM.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }
        let h = sched::create_thread(t5_signaller, core::ptr::null_mut())
            .expect("test 5: spawn signaller");

        let res = sem.wait_with_timeout(2000, false).is_ok();

        h.wait(false);
        assert!(res, "test 5: must return true (signalled), got false (timeout)");
    }
    info!("sync test 5: PASSED");

    // Test 6: multi-waiter FIFO + per-task timer isolation
    //
    // Three waiters all wait_with_timeout on the same KSem with
    // very different timeouts. We then signal twice, then wait long enough
    // for the third's timer to expire. Expectation:
    //   - 2 of the long-timeout waiters wake with success
    //   - the short-timeout waiter times out
    //   - no other "timeout" is recorded by the long waiters
    info!("sync test 6: per-task timer isolation under signal");
    {
        reset_sync_counters();
        let sem = KSem::new(0, 4);
        static T6_SEM: Once<KSem> = Once::new();
        T6_SEM.call_once(|| sem.clone());

        extern "C" fn t6_long() -> ! {
            let res = T6_SEM.get().unwrap().wait_with_timeout(3000, false).is_ok();
            if res { SYNC_SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed); }
            else   { SYNC_TIMEOUT_COUNT.fetch_add(1, Ordering::Relaxed); }
            SYNC_WAKE_COUNT.fetch_add(1, Ordering::Relaxed);
            SYNC_TEST_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }
        extern "C" fn t6_short() -> ! {
            let res = T6_SEM.get().unwrap().wait_with_timeout(200, false).is_ok();
            if res { SYNC_SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed); }
            else   { SYNC_TIMEOUT_COUNT.fetch_add(1, Ordering::Relaxed); }
            SYNC_WAKE_COUNT.fetch_add(1, Ordering::Relaxed);
            SYNC_TEST_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }

        let l1 = sched::create_thread(t6_long,  core::ptr::null_mut()).unwrap();
        let l2 = sched::create_thread(t6_long,  core::ptr::null_mut()).unwrap();
        let s1 = sched::create_thread(t6_short, core::ptr::null_mut()).unwrap();

        // Make sure all three blocked.
        sched::delay_ms(50, false);

        // Two signals → wakes 2 of the 3 
        sem.signal();
        sem.signal();

        // Wait for all three workers.
        join_workers(SYNC_TEST_DONE.get().unwrap(), 3);
        l1.wait(false); l2.wait(false); s1.wait(false);

        let signalled = SYNC_SIGNAL_COUNT.load(Ordering::Relaxed);
        let timed_out = SYNC_TIMEOUT_COUNT.load(Ordering::Relaxed);
        assert!(signalled == 2,
            "test 6: signalled count = {}, expected 2", signalled);
        assert!(timed_out == 1,
            "test 6: timeout count = {}, expected 1 (regression: timer-by-wrong-task removal)", timed_out);
        info!("sync test 6: PASSED (signalled={}, timed_out={})", signalled, timed_out);
    }

    // Test 7: multi-waiter — N waiters, M < N signals → only M wake
    info!("sync test 7: multi-waiter partial wake");
    {
        reset_sync_counters();
        let sem = KSem::new(0, 8);
        static T7_SEM: Once<KSem> = Once::new();
        T7_SEM.call_once(|| sem.clone());

        extern "C" fn t7_waiter() -> ! {
            // 2s timeout so unsignalled waiters eventually time out and the
            // test finishes (we want them all to die so we can read counts).
            let res = T7_SEM.get().unwrap().wait_with_timeout(1500, false).is_ok();
            if res { SYNC_SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed); }
            else   { SYNC_TIMEOUT_COUNT.fetch_add(1, Ordering::Relaxed); }
            SYNC_WAKE_COUNT.fetch_add(1, Ordering::Relaxed);
            SYNC_TEST_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }

        let mut handles = alloc::vec::Vec::new();
        for _ in 0..5 {
            handles.push(sched::create_thread(t7_waiter, core::ptr::null_mut()).unwrap());
        }
        sched::delay_ms(50, false);

        // Signal 3 of 5 waiters.
        sem.signal();
        sem.signal();
        sem.signal();

        join_workers(SYNC_TEST_DONE.get().unwrap(), 5);
        for h in &handles { h.wait(false); }

        let signalled = SYNC_SIGNAL_COUNT.load(Ordering::Relaxed);
        let timed_out = SYNC_TIMEOUT_COUNT.load(Ordering::Relaxed);
        assert!(signalled == 3, "test 7: signalled = {}, expected 3", signalled);
        assert!(timed_out == 2, "test 7: timed_out = {}, expected 2", timed_out);
        info!("sync test 7: PASSED (signalled={}, timed_out={})", signalled, timed_out);
    }

    // Test 8: manual-reset KEvent — signal once, all waiters wake, and
    // subsequent waits also acquire without blocking.
    info!("sync test 8: manual-reset event");
    {
        reset_sync_counters();
        let ev = KEvent::new(false);  // manual reset
        static T8_EV: Once<KEvent> = Once::new();
        T8_EV.call_once(|| ev.clone());

        extern "C" fn t8_waiter() -> ! {
            T8_EV.get().unwrap().wait(false);
            SYNC_WAKE_COUNT.fetch_add(1, Ordering::Relaxed);
            SYNC_TEST_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }

        let mut hs = alloc::vec::Vec::new();
        for _ in 0..4 {
            hs.push(sched::create_thread(t8_waiter, core::ptr::null_mut()).unwrap());
        }
        sched::delay_ms(50, false);

        ev.signal();

        join_workers(SYNC_TEST_DONE.get().unwrap(), 4);
        for h in &hs { h.wait(false); }

        // After signal, manual event stays set: a fresh wait must succeed.
        assert!(ev.wait_with_timeout(0, false).is_ok(),
            "test 8: try_wait on already-signalled manual event must succeed");
        // Reset must un-signal it.
        ev.reset();
        assert!(!ev.wait_with_timeout(0, false).is_ok(),
            "test 8: try_wait on reset manual event must fail");

        let woken = SYNC_WAKE_COUNT.load(Ordering::Relaxed);
        assert!(woken == 4, "test 8: woke {} threads, expected 4", woken);
        info!("sync test 8: PASSED (woke {})", woken);
    }

    // Test 9: auto-reset KEvent — signal N times, exactly N waiters wake.
    info!("sync test 9: auto-reset event");
    {
        reset_sync_counters();
        let ev = KEvent::new(true); // auto reset
        static T9_EV: Once<KEvent> = Once::new();
        T9_EV.call_once(|| ev.clone());

        extern "C" fn t9_waiter() -> ! {
            // Bounded timeout so test exits even if signalling under-counts.
            let res = T9_EV.get().unwrap().wait_with_timeout(1500, false).is_ok();
            if res { SYNC_SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed); }
            else   { SYNC_TIMEOUT_COUNT.fetch_add(1, Ordering::Relaxed); }
            SYNC_WAKE_COUNT.fetch_add(1, Ordering::Relaxed);
            SYNC_TEST_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }

        let mut hs = alloc::vec::Vec::new();
        for _ in 0..4 {
            hs.push(sched::create_thread(t9_waiter, core::ptr::null_mut()).unwrap());
        }
        sched::delay_ms(50, false);

        // Signal twice with a tiny gap so consume-and-signal races resolve.
        ev.signal();
        sched::delay_ms(20, false);
        ev.signal();

        join_workers(SYNC_TEST_DONE.get().unwrap(), 4);
        for h in &hs { h.wait(false); }

        let signalled = SYNC_SIGNAL_COUNT.load(Ordering::Relaxed);
        let timed_out = SYNC_TIMEOUT_COUNT.load(Ordering::Relaxed);
        assert!(signalled == 2,
            "test 9: signalled = {}, expected 2 (auto-reset consumes per wake)", signalled);
        assert!(timed_out == 2,
            "test 9: timed_out = {}, expected 2", timed_out);
        info!("sync test 9: PASSED (signalled={}, timed_out={})", signalled, timed_out);
    }

    // Test 10: try_wait on KEvent — manual + auto behaviour.
    info!("sync test 10: KEvent try_wait semantics");
    {
        // Manual: signalled stays raised; auto: signalled gets consumed by
        // a real wait, but try_wait (timeout=0) per current implementation
        // does NOT consume (it just reports state).
        let manual = KEvent::new(false);
        let auto   = KEvent::new(true);

        assert!(!manual.wait_with_timeout(0, false).is_ok(), "test 10: manual unsignalled try_wait → false");
        assert!(!auto.wait_with_timeout(0, false).is_ok(),   "test 10: auto unsignalled try_wait → false");

        manual.signal();
        auto.signal();
        assert!(manual.wait_with_timeout(0, false).is_ok(), "test 10: manual signalled try_wait → true");
        assert!(auto.wait_with_timeout(0, false).is_ok(),   "test 10: auto signalled try_wait → true");

        // Manual: still signalled after try_wait.
        assert!(manual.wait_with_timeout(0, false).is_ok(), "test 10: manual still signalled after try_wait");
        // Auto: try_wait does NOT consume signal (per current code: returns
        // *signalled). A real wait() would consume it. We document this.
        assert!(!auto.wait_with_timeout(0, false).is_ok(), "auto event consumed — second try_wait → false")
    }
    info!("sync test 10: PASSED");

    // Test 11: stress — high-volume signal/wait on one semaphore.
    info!("sync test 11: stress signal/wait");
    {
        reset_sync_counters();
        const N: usize = 200;
        let sem = KSem::new(0, N as isize);
        static T11_SEM: Once<KSem> = Once::new();
        T11_SEM.call_once(|| sem.clone());

        extern "C" fn t11_waiter() -> ! {
            let res = T11_SEM.get().unwrap().wait_with_timeout(2000, false).is_ok();
            if res { SYNC_SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed); }
            else   { SYNC_TIMEOUT_COUNT.fetch_add(1, Ordering::Relaxed); }
            SYNC_TEST_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }
        extern "C" fn t11_signaller() -> ! {
            for _ in 0..N {
                T11_SEM.get().unwrap().signal();
            }
            sched::exit_thread(ExitInfo::normal(0));
        }

        let mut waiters = alloc::vec::Vec::new();
        for _ in 0..N {
            waiters.push(sched::create_thread(t11_waiter, core::ptr::null_mut()).unwrap());
        }
        let signaller = sched::create_thread(t11_signaller, core::ptr::null_mut()).unwrap();

        join_workers(SYNC_TEST_DONE.get().unwrap(), N);
        for h in &waiters { h.wait(false); }
        signaller.wait(false);

        let signalled = SYNC_SIGNAL_COUNT.load(Ordering::Relaxed);
        let timed_out = SYNC_TIMEOUT_COUNT.load(Ordering::Relaxed);
        assert!(signalled == N,
            "test 11: signalled = {}, expected {} (regression: counter drift on signal/timer race)",
            signalled, N);
        assert!(timed_out == 0,
            "test 11: timed_out = {}, expected 0", timed_out);
        info!("sync test 11: PASSED (signalled={}, timed_out={})", signalled, timed_out);
    }

    info!("=== run_sync_tests: PASSED ===");
}

#[cfg(feature = "kunit-test")]
mod test_fs_conc {
    use super::*;

    pub static FS_CONC_DONE: Once<KSem> = Once::new();
    pub static FS_CONC_INDEX: AtomicUsize = AtomicUsize::new(0);
    pub static FS_CONC_COUNTER: AtomicUsize = AtomicUsize::new(0);
}

#[cfg(feature = "kunit-test")]
use test_fs_conc::*;

// Exercises the fs-layer correctness (mount canonicalization, cross-
// mount symlink resolution, FAT32 FFI bool marshaling, busy-checks and
// mount-point checks on delete/rename) against the real FAT32 root that
// fs::load_root_fs() has already mounted by the time run_tests!() fires,
// plus scratch in-memory mounts created via fs::new_memory_source().
#[kmod::test_function(true)]
fn run_fs_correctness_tests() {
    info!("fs test 1: module-backend busy check on delete/rename");
    {
        fs::mkdir("/t1", 0).expect("test 1: mkdir /t1");
        fs::create_file("/t1/a.txt", 0).expect("test 1: create a.txt");
        fs::create_file("/t1/b.txt", 0).expect("test 1: create b.txt");
        let handle = fs::open("/t1/a.txt").expect("test 1: open a.txt");

        let del_res = fs::delete("/t1/a.txt");
        assert!(matches!(del_res, Err(KError::FileBusy)),
            "test 1: delete of open file should be FileBusy, got {:?}", del_res);

        let ren_res = fs::rename("/t1/a.txt", "/t1/c.txt");
        assert!(matches!(ren_res, Err(KError::FileBusy)),
            "test 1: rename of open file should be FileBusy, got {:?}", ren_res);

        fs::rename("/t1/b.txt", "/t1/d.txt").expect("test 1: rename of non-open file should succeed");

        drop(handle);
        fs::delete("/t1/a.txt").expect("test 1: delete after close should succeed");
    }
    info!("fs test 1: PASSED");

    info!("fs test 2: mount-point protection on delete/rename (both sides)");
    {
        fs::mkdir("/mnt2", 0).expect("test 2: mkdir /mnt2");
        fs::mount("/mnt2", fs::new_memory_source()).expect("test 2: mount /mnt2");
        fs::mkdir("/other", 0).expect("test 2: mkdir /other");
        fs::create_file("/other/file.txt", 0).expect("test 2: create /other/file.txt");

        let del_res = fs::delete("/mnt2");
        assert!(matches!(del_res, Err(KError::FileBusy)),
            "test 2: delete of mount point should be FileBusy, got {:?}", del_res);

        let ren_from_res = fs::rename("/mnt2", "/other/x");
        assert!(matches!(ren_from_res, Err(KError::FileBusy)),
            "test 2: rename FROM a mount point should be FileBusy, got {:?}", ren_from_res);

        let ren_to_res = fs::rename("/other/file.txt", "/mnt2");
        assert!(matches!(ren_to_res, Err(KError::FileBusy)),
            "test 2: rename TO a mount point should be FileBusy, got {:?}", ren_to_res);
    }
    info!("fs test 2: PASSED");

    info!("fs test 3: cross-mount symlink resolution (both directions)");
    {
        fs::mkdir("/xmnt", 0).expect("test 3: mkdir /xmnt");
        fs::mount("/xmnt", fs::new_memory_source()).expect("test 3: mount /xmnt");
        fs::mkdir("/xmnt/inner", 0).expect("test 3: mkdir /xmnt/inner");
        fs::create_file("/xmnt/inner/target.txt", 0).expect("test 3: create target.txt");

        fs::create_symlink("/link_out", "/xmnt/inner/target.txt").expect("test 3: create /link_out");

        let attrs = fs::stat("/link_out").expect("test 3: stat /link_out should cross into the memory mount");
        assert!(attrs.mode & fs::MODE_FILE != 0,
            "test 3: /link_out should resolve to a file, mode={:#x}", attrs.mode);

        let canonical = fs::resolve_symlink("/link_out").expect("test 3: resolve_symlink /link_out");
        assert_eq!(canonical, "/xmnt/inner/target.txt",
            "test 3: canonical path mismatch, got {}", canonical);

        let h = fs::open("/link_out").expect("test 3: open /link_out");
        assert!(!h.is_dir(), "test 3: /link_out should open as a file");
        drop(h);

        fs::create_symlink("/xmnt/link_back", "/other/file.txt").expect("test 3: create /xmnt/link_back");
        fs::stat("/xmnt/link_back").expect("test 3: stat /xmnt/link_back should cross back into the fat32 root");
    }
    info!("fs test 3: PASSED");

    info!("fs test 4: mount canonicalization");
    {
        fs::mkdir("/cdir", 0).expect("test 4: mkdir /cdir");
        fs::create_file("/cdir/fat_only.txt", 0).expect("test 4: create /cdir/fat_only.txt");
        fs::create_symlink("/cdirlink", "/cdir").expect("test 4: create /cdirlink -> /cdir");

        fs::mount("/cdirlink", fs::new_memory_source()).expect("test 4: mount /cdirlink");

        let stat_res = fs::stat("/cdir/fat_only.txt");
        assert!(matches!(stat_res, Err(KError::NotFound)),
            "test 4: /cdir/fat_only.txt should be shadowed by the new mount (mount point must have been \
             canonicalized to /cdir, not stored as /cdirlink), got {:?}", stat_res);

        fs::create_file("/cdir/mem_only.txt", 0).expect("test 4: create /cdir/mem_only.txt");
        fs::stat("/cdir/mem_only.txt").expect("test 4: stat /cdir/mem_only.txt");

        fs::create_file("/cdirlink/via_link.txt", 0).expect("test 4: create /cdirlink/via_link.txt");
        fs::stat("/cdir/via_link.txt").expect("test 4: stat /cdir/via_link.txt (same backend, reached both ways)");
    }
    info!("fs test 4: PASSED");

    info!("fs test 5: FFI bool marshaling (mode reported correctly for dirs vs files)");
    {
        fs::mkdir("/t1/sub", 0).expect("test 5: mkdir /t1/sub");
        let attrs = fs::stat("/t1/sub").expect("test 5: stat /t1/sub");
        assert!(attrs.mode & fs::MODE_DIR != 0, "test 5: /t1/sub should report MODE_DIR, mode={:#x}", attrs.mode);
        assert!(attrs.mode & fs::MODE_FILE == 0, "test 5: /t1/sub should not report MODE_FILE, mode={:#x}", attrs.mode);
    }
    info!("fs test 5: PASSED");

    info!("fs test 6: read/write roundtrip on the fat32-backed root");
    {
        fs::create_file("/t1/rw.txt", 0).expect("test 6: create /t1/rw.txt");
        let handle = fs::open("/t1/rw.txt").expect("test 6: open /t1/rw.txt");

        let write_data = b"Hello, Archis FS! This is a fat32 read/write roundtrip test.";
        let wbuf = fs::FileBuffer::new(write_data.len(), false).expect("test 6: alloc write buffer");
        wbuf.write(write_data.as_ptr() as usize, write_data.len(), 0).expect("test 6: fill write buffer");
        let written = handle.write(&wbuf, write_data.len(), 0).expect("test 6: write /t1/rw.txt");
        assert_eq!(written, write_data.len(), "test 6: short write to /t1/rw.txt");

        let attrs = fs::stat("/t1/rw.txt").expect("test 6: stat /t1/rw.txt after write");
        assert_eq!(attrs.size, write_data.len() as u64,
            "test 6: /t1/rw.txt size mismatch after write, got {}", attrs.size);

        handle.seek(0).expect("test 6: seek to 0");
        let rbuf = fs::FileBuffer::new(write_data.len(), false).expect("test 6: alloc read buffer");
        let read_len = handle.read(&rbuf).expect("test 6: read /t1/rw.txt");
        assert_eq!(read_len, write_data.len(), "test 6: short read from /t1/rw.txt");

        let mut out = vec![0u8; write_data.len()];
        rbuf.read(out.as_mut_ptr() as usize, read_len, 0).expect("test 6: drain read buffer");
        assert_eq!(&out[..], &write_data[..], "test 6: readback mismatch on /t1/rw.txt");
        drop(handle);

        // Re-open and read again to make sure the write actually persisted to
        // disk (patch_dir_entry updated the on-disk size/cluster), not just
        // to the still-open handle's cached state.
        let handle2 = fs::open("/t1/rw.txt").expect("test 6: reopen /t1/rw.txt");
        let rbuf2 = fs::FileBuffer::new(write_data.len(), false).expect("test 6: alloc reopen read buffer");
        let read_len2 = handle2.read(&rbuf2).expect("test 6: read /t1/rw.txt after reopen");
        assert_eq!(read_len2, write_data.len(), "test 6: short read from /t1/rw.txt after reopen");
        let mut out2 = vec![0u8; write_data.len()];
        rbuf2.read(out2.as_mut_ptr() as usize, read_len2, 0).expect("test 6: drain reopen read buffer");
        assert_eq!(&out2[..], &write_data[..], "test 6: readback mismatch on /t1/rw.txt after reopen");
        drop(handle2);
    }
    info!("fs test 6: PASSED");

    info!("fs test 7: read/write roundtrip on a memory-backed mount");
    {
        fs::mkdir("/rwmnt", 0).expect("test 7: mkdir /rwmnt");
        fs::mount("/rwmnt", fs::new_memory_source()).expect("test 7: mount /rwmnt");
        fs::create_file("/rwmnt/mem.txt", 0).expect("test 7: create /rwmnt/mem.txt");
        let handle = fs::open("/rwmnt/mem.txt").expect("test 7: open /rwmnt/mem.txt");

        let write_data = b"Hello from the in-memory backend.";
        let wbuf = fs::FileBuffer::new(write_data.len(), false).expect("test 7: alloc write buffer");
        wbuf.write(write_data.as_ptr() as usize, write_data.len(), 0).expect("test 7: fill write buffer");
        let written = handle.write(&wbuf, write_data.len(), 0).expect("test 7: write /rwmnt/mem.txt");
        assert_eq!(written, write_data.len(), "test 7: short write to /rwmnt/mem.txt");

        handle.seek(0).expect("test 7: seek to 0");
        let rbuf = fs::FileBuffer::new(write_data.len(), false).expect("test 7: alloc read buffer");
        let read_len = handle.read(&rbuf).expect("test 7: read /rwmnt/mem.txt");
        assert_eq!(read_len, write_data.len(), "test 7: short read from /rwmnt/mem.txt");
        let mut out = vec![0u8; write_data.len()];
        rbuf.read(out.as_mut_ptr() as usize, read_len, 0).expect("test 7: drain read buffer");
        assert_eq!(&out[..], &write_data[..], "test 7: readback mismatch on /rwmnt/mem.txt");
        drop(handle);
    }
    info!("fs test 7: PASSED");

    FS_CONC_DONE.call_once(|| KSem::new(0, 32));

    info!("fs test 8: concurrent writers to the same open file");
    {
        fs::create_file("/conc_w.txt", 0).expect("test 8: create /conc_w.txt");
        FS_CONC_INDEX.store(0, Ordering::Relaxed);

        const WORKERS: usize = 6;
        const CHUNK: usize = 16;

        // Every worker opens the file independently (its own fs_open call,
        // its own kernel-side FileInstance) but must land on the same
        // fat32-side SharedFileState, since they all resolve the same
        // (parent_cluster, slot_start)
        extern "C" fn conc_writer() -> ! {
            let idx = FS_CONC_INDEX.fetch_add(1, Ordering::Relaxed);
            let handle = fs::open("/conc_w.txt").expect("test 8: worker open");
            handle.seek(idx * 16).expect("test 8: worker seek");
            let data = [b'A' + idx as u8; 16];
            let wbuf = fs::FileBuffer::new(16, false).expect("test 8: worker alloc buffer");
            wbuf.write(data.as_ptr() as usize, 16, 0).expect("test 8: worker fill buffer");
            handle.write(&wbuf, 16, 0).expect("test 8: worker write");
            drop(handle);
            FS_CONC_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }

        let mut handles = alloc::vec::Vec::new();
        for _ in 0..WORKERS {
            handles.push(sched::create_thread(conc_writer, core::ptr::null_mut()).expect("test 8: spawn worker"));
        }
        join_workers(FS_CONC_DONE.get().unwrap(), WORKERS);
        for h in &handles { h.wait(false); }

        let attrs = fs::stat("/conc_w.txt").expect("test 8: stat /conc_w.txt");
        assert_eq!(attrs.size, (WORKERS * CHUNK) as u64,
            "test 8: size mismatch (some writes were lost), got {}", attrs.size);

        let verify_handle = fs::open("/conc_w.txt").expect("test 8: reopen for verify");
        let rbuf = fs::FileBuffer::new(WORKERS * CHUNK, false).expect("test 8: alloc verify buffer");
        let read_len = verify_handle.read(&rbuf).expect("test 8: read all bytes");
        assert_eq!(read_len, WORKERS * CHUNK, "test 8: short read on verify");
        let mut out = vec![0u8; WORKERS * CHUNK];
        rbuf.read(out.as_mut_ptr() as usize, read_len, 0).expect("test 8: drain verify buffer");
        for i in 0..WORKERS {
            let expected = b'A' + i as u8;
            for b in &out[i * CHUNK..(i + 1) * CHUNK] {
                assert_eq!(*b, expected, "test 8: worker {}'s region corrupted by another writer", i);
            }
        }
        drop(verify_handle);
        fs::delete("/conc_w.txt").expect("test 8: cleanup /conc_w.txt");
    }
    info!("fs test 8: PASSED");

    info!("fs test 9: concurrent creates in the same directory");
    {
        fs::mkdir("/conc_dir", 0).expect("test 9: mkdir /conc_dir");
        FS_CONC_INDEX.store(0, Ordering::Relaxed);

        const CREATORS: usize = 8;

        extern "C" fn conc_creator() -> ! {
            let idx = FS_CONC_INDEX.fetch_add(1, Ordering::Relaxed);
            let path = alloc::format!("/conc_dir/f{}.txt", idx);
            fs::create_file(&path, 0).expect("test 9: worker create");
            FS_CONC_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }

        let mut handles = alloc::vec::Vec::new();
        for _ in 0..CREATORS {
            handles.push(sched::create_thread(conc_creator, core::ptr::null_mut()).expect("test 9: spawn worker"));
        }
        join_workers(FS_CONC_DONE.get().unwrap(), CREATORS);
        for h in &handles { h.wait(false); }

        // If the per-directory lock's critical section didn't cover the
        // whole duplicate-check-then-append sequence, two concurrent
        // creates could both read the same end-of-entries marker and the
        // second write-back would silently clobber the first's new entry.
        for i in 0..CREATORS {
            let path = alloc::format!("/conc_dir/f{}.txt", i);
            fs::stat(&path).unwrap_or_else(|e| panic!("test 9: {} missing after concurrent create (lost create), err={:?}", path, e));
        }

        for i in 0..CREATORS {
            let path = alloc::format!("/conc_dir/f{}.txt", i);
            fs::delete(&path).expect("test 9: cleanup");
        }
        fs::delete("/conc_dir").expect("test 9: cleanup /conc_dir");
    }
    info!("fs test 9: PASSED");

    info!("fs test 10: symlink open/stat/resolve survive concurrent directory churn");
    {
        fs::create_file("/conc_target.txt", 0).expect("test 10: create /conc_target.txt");
        fs::create_symlink("/conc_link", "/conc_target.txt").expect("test 10: create /conc_link");
        FS_CONC_INDEX.store(0, Ordering::Relaxed);

        const READERS: usize = 3;
        const CHURNERS: usize = 3;
        const ITERS: usize = 20;

        // Readers repeatedly stat/resolve/open+close through the symlink;
        // churners concurrently create+delete unrelated files at the same
        // root directory the symlink lives in, so both groups contend on
        // the same directory lock the whole time. Nothing here should ever
        // panic, return a corrupted target, or momentarily see a mode other
        // than MODE_FILE 
        extern "C" fn conc_reader() -> ! {
            for _ in 0..ITERS {
                let attrs = fs::stat("/conc_link").expect("test 10: reader stat /conc_link");
                assert!(attrs.mode & fs::MODE_FILE != 0, "test 10: /conc_link should resolve to a file");
                let target = fs::resolve_symlink("/conc_link").expect("test 10: reader resolve_symlink");
                assert_eq!(target, "/conc_target.txt", "test 10: symlink target corrupted under concurrent churn, got {}", target);
                let handle = fs::open("/conc_link").expect("test 10: reader open through symlink");
                assert!(!handle.is_dir(), "test 10: /conc_link should open as a file");
                drop(handle);
            }
            FS_CONC_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }

        extern "C" fn conc_churner() -> ! {
            let idx = FS_CONC_INDEX.fetch_add(1, Ordering::Relaxed);
            for i in 0..ITERS {
                let path = alloc::format!("/churn_{}_{}.txt", idx, i);
                fs::create_file(&path, 0).expect("test 10: churner create");
                fs::delete(&path).expect("test 10: churner delete");
            }
            FS_CONC_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }

        let mut handles = alloc::vec::Vec::new();
        for _ in 0..READERS {
            handles.push(sched::create_thread(conc_reader, core::ptr::null_mut()).expect("test 10: spawn reader"));
        }
        for _ in 0..CHURNERS {
            handles.push(sched::create_thread(conc_churner, core::ptr::null_mut()).expect("test 10: spawn churner"));
        }
        join_workers(FS_CONC_DONE.get().unwrap(), READERS + CHURNERS);
        for h in &handles { h.wait(false); }

        fs::delete("/conc_link").expect("test 10: cleanup /conc_link");
        fs::delete("/conc_target.txt").expect("test 10: cleanup /conc_target.txt");
    }
    info!("fs test 10: PASSED");

    info!("fs test 11: concurrent delete race on the same file (regression: no double-free/corruption)");
    {
        fs::create_file("/conc_del_race.txt", 0).expect("test 11: create /conc_del_race.txt");
        FS_CONC_COUNTER.store(0, Ordering::Relaxed);

        const RACERS: usize = 6;

        // Every racer independently resolves and tries to delete the exact
        // same file. Before the resolve-then-act fix, both resolves could
        // succeed before either took the lock, letting free_chain run twice
        // on the same (possibly-already-reallocated) chain. Now resolve
        // holds the parent's lock continuously through the delete, so
        // exactly one racer's delete can ever see the entry.
        extern "C" fn conc_deleter() -> ! {
            if fs::delete("/conc_del_race.txt").is_ok() {
                FS_CONC_COUNTER.fetch_add(1, Ordering::Relaxed);
            }
            FS_CONC_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }

        let mut handles = alloc::vec::Vec::new();
        for _ in 0..RACERS {
            handles.push(sched::create_thread(conc_deleter, core::ptr::null_mut()).expect("test 11: spawn deleter"));
        }
        join_workers(FS_CONC_DONE.get().unwrap(), RACERS);
        for h in &handles { h.wait(false); }

        let successes = FS_CONC_COUNTER.load(Ordering::Relaxed);
        assert_eq!(successes, 1,
            "test 11: exactly one concurrent delete of the same file should succeed, got {}", successes);

        // Filesystem must still be healthy afterward -- if a chain had been
        // double-freed/corrupted, this would misbehave.
        fs::create_file("/conc_del_race.txt", 0).expect("test 11: fs still healthy after race, recreate should succeed");
        let handle = fs::open("/conc_del_race.txt").expect("test 11: open after recreate");
        drop(handle);
        fs::delete("/conc_del_race.txt").expect("test 11: cleanup");
    }
    info!("fs test 11: PASSED");

    info!("fs test 12: rename to an existing destination leaves the source intact (regression: data-loss-on-failure)");
    {
        // Same-directory case.
        fs::create_file("/ren_src.txt", 0).expect("test 12: create /ren_src.txt");
        let handle = fs::open("/ren_src.txt").expect("test 12: open /ren_src.txt");
        let data = b"original content survives a failed rename";
        let wbuf = fs::FileBuffer::new(data.len(), false).expect("test 12: alloc write buffer");
        wbuf.write(data.as_ptr() as usize, data.len(), 0).expect("test 12: fill write buffer");
        handle.write(&wbuf, data.len(), 0).expect("test 12: write /ren_src.txt");
        drop(handle);
        fs::create_file("/ren_dst.txt", 0).expect("test 12: create /ren_dst.txt");

        let ren_res = fs::rename("/ren_src.txt", "/ren_dst.txt");
        assert!(matches!(ren_res, Err(KError::FileExists)),
            "test 12: rename to an existing destination should fail with FileExists, got {:?}", ren_res);

        let attrs = fs::stat("/ren_src.txt").expect("test 12: /ren_src.txt should still exist after failed rename");
        assert_eq!(attrs.size, data.len() as u64, "test 12: /ren_src.txt size wrong after failed rename");

        let verify_handle = fs::open("/ren_src.txt").expect("test 12: reopen /ren_src.txt");
        let rbuf = fs::FileBuffer::new(data.len(), false).expect("test 12: alloc read buffer");
        let read_len = verify_handle.read(&rbuf).expect("test 12: read /ren_src.txt");
        assert_eq!(read_len, data.len(), "test 12: short read on /ren_src.txt after failed rename");
        let mut out = vec![0u8; data.len()];
        rbuf.read(out.as_mut_ptr() as usize, read_len, 0).expect("test 12: drain read buffer");
        assert_eq!(&out[..], &data[..], "test 12: /ren_src.txt content corrupted after failed rename");
        drop(verify_handle);

        fs::delete("/ren_src.txt").expect("test 12: cleanup /ren_src.txt");
        fs::delete("/ren_dst.txt").expect("test 12: cleanup /ren_dst.txt");

        // Cross-directory case -- exercises the destination-first,
        // source-delete-last path specifically.
        fs::mkdir("/ren_dstdir", 0).expect("test 12: mkdir /ren_dstdir");
        fs::create_file("/ren_src2.txt", 0).expect("test 12: create /ren_src2.txt");
        fs::create_file("/ren_dstdir/taken.txt", 0).expect("test 12: create /ren_dstdir/taken.txt");

        let ren_res2 = fs::rename("/ren_src2.txt", "/ren_dstdir/taken.txt");
        assert!(matches!(ren_res2, Err(KError::FileExists)),
            "test 12: cross-dir rename to an existing destination should fail with FileExists, got {:?}", ren_res2);
        fs::stat("/ren_src2.txt").expect("test 12: /ren_src2.txt should still exist after failed cross-dir rename");

        fs::delete("/ren_src2.txt").expect("test 12: cleanup /ren_src2.txt");
        fs::delete("/ren_dstdir/taken.txt").expect("test 12: cleanup /ren_dstdir/taken.txt");
        fs::delete("/ren_dstdir").expect("test 12: cleanup /ren_dstdir");
    }
    info!("fs test 12: PASSED");

    info!("fs test 13: symlink is never visible with placeholder metadata (regression: create-then-patch race)");
    {
        fs::create_file("/symrace_target.txt", 0).expect("test 13: create /symrace_target.txt");

        const ITERS: usize = 8;
        const READERS: usize = 2;

        extern "C" fn symrace_creator() -> ! {
            for _ in 0..ITERS {
                let _ = fs::create_symlink("/symrace_link", "/symrace_target.txt");
                let _ = fs::delete("/symrace_link");
            }
            FS_CONC_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }

        // Whenever a reader observes /symrace_link as a symlink at all, its
        // target must already be the full, correct string -- never the
        // placeholder 0/0-metadata state the old create-then-patch sequence
        // could expose to a concurrent resolver.
        extern "C" fn symrace_reader() -> ! {
            for _ in 0..ITERS {
                if let Ok((attrs, target)) = fs::lstat("/symrace_link") {
                    if attrs.mode & fs::MODE_SYMLINK != 0 {
                        let target = target.expect("test 13: symlink lstat must report a target");
                        assert_eq!(target, "/symrace_target.txt",
                            "test 13: symlink target corrupted/truncated mid-creation, got {:?}", target);
                    }
                }
            }
            FS_CONC_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }

        let mut handles = alloc::vec::Vec::new();
        handles.push(sched::create_thread(symrace_creator, core::ptr::null_mut()).expect("test 13: spawn creator"));
        for _ in 0..READERS {
            handles.push(sched::create_thread(symrace_reader, core::ptr::null_mut()).expect("test 13: spawn reader"));
        }
        join_workers(FS_CONC_DONE.get().unwrap(), 1 + READERS);
        for h in &handles { h.wait(false); }

        let _ = fs::delete("/symrace_link");
        fs::delete("/symrace_target.txt").expect("test 13: cleanup /symrace_target.txt");
    }
    info!("fs test 13: PASSED");

    info!("fs test 14: readdir returns . and .. first, for both backends and root (regression: dot-entry inconsistency)");
    {
        fn check_dots(handle: &fs::FileInstance) {
            let e0 = handle.readdir_at(0).expect("test 14: readdir offset 0");
            assert_eq!(e0.name, ".", "test 14: offset 0 should be '.', got {}", e0.name);
            let e1 = handle.readdir_at(1).expect("test 14: readdir offset 1");
            assert_eq!(e1.name, "..", "test 14: offset 1 should be '..', got {}", e1.name);
        }

        // FAT32-backed root -- the one case with no real on-disk dot
        // entries at all (do_format never writes them for root).
        let root_handle = fs::open("/").expect("test 14: open /");
        check_dots(&root_handle);
        drop(root_handle);

        // FAT32-backed non-root directory -- real on-disk dot entries.
        fs::mkdir("/dotcheck_fat", 0).expect("test 14: mkdir /dotcheck_fat");
        let fat_handle = fs::open("/dotcheck_fat").expect("test 14: open /dotcheck_fat");
        check_dots(&fat_handle);
        drop(fat_handle);
        fs::delete("/dotcheck_fat").expect("test 14: cleanup /dotcheck_fat");

        // Memory-backed mount, both its root and a subdirectory -- neither
        // stores dot entries at all, both must be synthesized.
        fs::mkdir("/dotcheck_mem", 0).expect("test 14: mkdir /dotcheck_mem");
        fs::mount("/dotcheck_mem", fs::new_memory_source()).expect("test 14: mount /dotcheck_mem");
        let mem_root_handle = fs::open("/dotcheck_mem").expect("test 14: open /dotcheck_mem");
        check_dots(&mem_root_handle);
        drop(mem_root_handle);

        fs::mkdir("/dotcheck_mem/sub", 0).expect("test 14: mkdir /dotcheck_mem/sub");
        let mem_sub_handle = fs::open("/dotcheck_mem/sub").expect("test 14: open /dotcheck_mem/sub");
        check_dots(&mem_sub_handle);
        drop(mem_sub_handle);

        fs::unmount("/dotcheck_mem").expect("test 14: unmount /dotcheck_mem");
        fs::delete("/dotcheck_mem").expect("test 14: cleanup /dotcheck_mem");
    }
    info!("fs test 14: PASSED");

    info!("fs test 15: concurrent renames in opposite directions between the same two directories never deadlock (regression: dual-lock ordering)");
    {
        fs::mkdir("/rendir_a", 0).expect("test 15: mkdir /rendir_a");
        fs::mkdir("/rendir_b", 0).expect("test 15: mkdir /rendir_b");
        fs::create_file("/rendir_a/ping.txt", 0).expect("test 15: create /rendir_a/ping.txt");
        fs::create_file("/rendir_b/pong.txt", 0).expect("test 15: create /rendir_b/pong.txt");

        const ITERS: usize = 2;

        // Both threads lock the same two directories every iteration, but in
        // opposite from/to roles. If lock order were ever picked by role
        // (e.g. always lock "to" first) rather than a fixed per-path-pair
        // order, this reliably deadlocks. The dual-lock design orders purely
        // by (path length, case-folded string) -- a pure function of the two
        // paths -- so both threads always agree on which directory to lock
        // first regardless of which way either rename is going.
        extern "C" fn renamer_a_to_b() -> ! {
            for _ in 0..ITERS {
                fs::rename("/rendir_a/ping.txt", "/rendir_b/ping.txt").expect("test 15: a->b");
                fs::rename("/rendir_b/ping.txt", "/rendir_a/ping.txt").expect("test 15: b->a");
            }
            FS_CONC_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }
        extern "C" fn renamer_b_to_a() -> ! {
            for _ in 0..ITERS {
                fs::rename("/rendir_b/pong.txt", "/rendir_a/pong.txt").expect("test 15: b->a");
                fs::rename("/rendir_a/pong.txt", "/rendir_b/pong.txt").expect("test 15: a->b");
            }
            FS_CONC_DONE.get().unwrap().signal();
            sched::exit_thread(ExitInfo::normal(0));
        }

        let mut handles = alloc::vec::Vec::new();
        handles.push(sched::create_thread(renamer_a_to_b, core::ptr::null_mut()).expect("test 15: spawn a->b"));
        handles.push(sched::create_thread(renamer_b_to_a, core::ptr::null_mut()).expect("test 15: spawn b->a"));
        join_workers(FS_CONC_DONE.get().unwrap(), 2);
        for h in &handles { h.wait(false); }

        fs::stat("/rendir_a/ping.txt").expect("test 15: ping.txt should have ended back in /rendir_a");
        fs::stat("/rendir_b/pong.txt").expect("test 15: pong.txt should have ended back in /rendir_b");

        fs::delete("/rendir_a/ping.txt").expect("test 15: cleanup /rendir_a/ping.txt");
        fs::delete("/rendir_b/pong.txt").expect("test 15: cleanup /rendir_b/pong.txt");
        fs::delete("/rendir_a").expect("test 15: cleanup /rendir_a");
        fs::delete("/rendir_b").expect("test 15: cleanup /rendir_b");
    }
    info!("fs test 15: PASSED");

    info!("fs cleanup: removing everything created by the tests above");
    {
        // Unmount every scratch mount created during the tests before
        // touching the directories they were mounted over (a mount point
        // can't be deleted while still mounted). mount() canonicalizes
        // through symlinks before storing, so /cdirlink was actually
        // recorded as /cdir.
        fs::unmount("/rwmnt").expect("cleanup: unmount /rwmnt");
        fs::unmount("/cdir").expect("cleanup: unmount /cdirlink (stored canonically as /cdir)");
        fs::unmount("/xmnt").expect("cleanup: unmount /xmnt");
        fs::unmount("/mnt2").expect("cleanup: unmount /mnt2");

        fs::delete("/rwmnt").expect("cleanup: delete /rwmnt");

        fs::delete("/link_out").expect("cleanup: delete /link_out");
        fs::delete("/xmnt").expect("cleanup: delete /xmnt");

        fs::delete("/mnt2").expect("cleanup: delete /mnt2");
        fs::delete("/other/file.txt").expect("cleanup: delete /other/file.txt");
        fs::delete("/other").expect("cleanup: delete /other");

        fs::delete("/cdirlink").expect("cleanup: delete /cdirlink");
        fs::delete("/cdir/fat_only.txt").expect("cleanup: delete /cdir/fat_only.txt");
        fs::delete("/cdir").expect("cleanup: delete /cdir");

        fs::delete("/t1/rw.txt").expect("cleanup: delete /t1/rw.txt");
        fs::delete("/t1/d.txt").expect("cleanup: delete /t1/d.txt");
        fs::delete("/t1/sub").expect("cleanup: delete /t1/sub");
        fs::delete("/t1").expect("cleanup: delete /t1");

        for name in ["/rwmnt", "/link_out", "/xmnt", "/mnt2", "/other", "/cdirlink", "/cdir", "/t1"] {
            assert!(matches!(fs::stat(name), Err(KError::NotFound)),
                "cleanup: {} should no longer exist", name);
        }
    }
    info!("fs cleanup: PASSED, root is clean");

    info!("=== run_fs_correctness_tests: PASSED ===");
}

fn kern_main() -> ! {
    info!("Starting main kernel init");

    mem::reclaim_pages();

    sync::init();
    pipe::init();
    sched::init();
    fs::init();
    loader::init();
    io::init();
    //#[cfg(feature = "kunit-test")]
    //fs::load_root_fs();

    kernel_intf::run_tests!();

    info!("Launching init...");
    let init_proc = sched::create_process(
        &["/bin/init"],
        &[],
        core::ptr::null_mut(),
        true,
        false
    ).expect("Failed to create init process!");

    init_proc.wait(false);
    panic!("init process returned!");
}

// === Driver worker (DPC) tests ===
//
// Verifies the new io_create_driver_worker / dw_handler path:
//   1. A queued DW actually runs, runs on the CPU it was queued on, runs with
//      interrupts enabled, and sees is_in_dw_mode() == true.
//   2. Several DWs queued back-to-back all execute and run to completion.
//   3. A DW queued from inside another DW is drained by the same dw_handler
//      loop (no need to wait for the next interrupt).
//   4. After everything drains, the CPU leaves DW mode.

#[cfg(feature = "kunit-test")]
use { 
    core::sync::atomic::AtomicI64,
    kernel_intf::SIGKILL,
    crate::sync::KEvent
};

#[cfg(feature = "kunit-test")]
mod dw_test_sync {
    use super::*;
    pub static DW_COUNTER:        AtomicI64    = AtomicI64::new(0);
    pub static DW_RAN:            AtomicUsize  = AtomicUsize::new(0);
    pub static DW_SAW_DW_MODE:    AtomicBool   = AtomicBool::new(false);
    pub static DW_SAW_INT_ENABLED:AtomicBool   = AtomicBool::new(false);
    pub static DW_RUN_CORE:       AtomicUsize  = AtomicUsize::new(usize::MAX);
}

#[cfg(feature = "kunit-test")]
pub use dw_test_sync::*;

#[cfg(feature = "kunit-test")]
extern "C" fn dw_test_routine(ctx: *mut c_void) {
    // ctx encodes a small i64 value to add to the counter.
    let v = ctx as usize as i64;
    DW_COUNTER.fetch_add(v, Ordering::Relaxed);
    DW_RAN.fetch_add(1, Ordering::Relaxed);

    if sched::is_in_dw_mode() {
        DW_SAW_DW_MODE.store(true, Ordering::Relaxed);
    }
    if hal::are_interrupts_enabled() {
        DW_SAW_INT_ENABLED.store(true, Ordering::Relaxed);
    }
    DW_RUN_CORE.store(hal::get_core(), Ordering::Relaxed);
}

#[cfg(feature = "kunit-test")]
extern "C" fn dw_chain_routine(ctx: *mut c_void) {
    let v = ctx as usize;
    DW_COUNTER.fetch_add(v as i64, Ordering::Relaxed);
    DW_RAN.fetch_add(1, Ordering::Relaxed);

    // First link of the chain queues a follow-up that adds 2. We confirm the
    // second link runs inside this same dw_handler drain loop (i.e. the chain
    // resolves without needing another hardware interrupt).
    if v == 1 {
        kernel_intf::io_create_driver_worker(dw_chain_routine, 2 as *mut c_void)
            .expect("chain DW queue failed");
    }
}

#[cfg(feature = "kunit-test")]
fn reset_dw_state() {
    DW_COUNTER.store(0, Ordering::Relaxed);
    DW_RAN.store(0, Ordering::Relaxed);
    DW_SAW_DW_MODE.store(false, Ordering::Relaxed);
    DW_SAW_INT_ENABLED.store(false, Ordering::Relaxed);
    DW_RUN_CORE.store(usize::MAX, Ordering::Relaxed);
}

#[kmod::test_function(false)]
fn run_driver_worker_tests() {
    info!("=== run_driver_worker_tests: BEGIN ===");

    assert!(!sched::is_in_dw_mode(), "is_in_dw_mode() true outside any DW?");

    // Test 1: single DW runs with expected invariants 
    reset_dw_state();
    let creator_core = hal::get_core();

    kernel_intf::io_create_driver_worker(dw_test_routine, 7 as *mut c_void)
        .expect("dw test 1: queue failed");

    // delay_ms blocks via semaphore wait; while we sleep the timer (or any
    // other interrupt) will land in global_interrupt_handler, which calls
    // dw_handler after the vector handler runs. The DW drains then.
    sched::delay_ms(50, false);

    assert!(DW_RAN.load(Ordering::Relaxed) >= 1,
        "dw test 1: DW did not run");
    assert!(DW_COUNTER.load(Ordering::Relaxed) == 7,
        "dw test 1: counter = {}, expected 7", DW_COUNTER.load(Ordering::Relaxed));
    assert!(DW_SAW_DW_MODE.load(Ordering::Relaxed),
        "dw test 1: is_in_dw_mode() was false inside DW");
    assert!(DW_SAW_INT_ENABLED.load(Ordering::Relaxed),
        "dw test 1: interrupts were disabled inside DW");
    assert!(DW_RUN_CORE.load(Ordering::Relaxed) == creator_core,
        "dw test 1: ran on core {}, queued from {}",
        DW_RUN_CORE.load(Ordering::Relaxed), creator_core);
    info!("dw test 1: PASSED (counter=7, core={}, dw_mode_seen=true, ints_on=true)",
        creator_core);

    // Test 2: batch of DWs all run 
    reset_dw_state();
    let values = [1i64, 2, 3, 4, 5];
    let expected: i64 = values.iter().sum();
    for v in values {
        kernel_intf::io_create_driver_worker(dw_test_routine, v as usize as *mut c_void)
            .expect("dw test 2: queue failed");
    }
    sched::delay_ms(50, false);

    let ran = DW_RAN.load(Ordering::Relaxed);
    let total = DW_COUNTER.load(Ordering::Relaxed);
    assert!(ran == values.len(),
        "dw test 2: ran {}, expected {}", ran, values.len());
    assert!(total == expected,
        "dw test 2: total {}, expected {}", total, expected);
    info!("dw test 2: PASSED (ran={}, total={})", ran, total);

    // Test 3: DW queued from inside a DW 
    reset_dw_state();
    kernel_intf::io_create_driver_worker(dw_chain_routine, 1 as *mut c_void)
        .expect("dw test 3: queue failed");
    sched::delay_ms(50, false);

    let total = DW_COUNTER.load(Ordering::Relaxed);
    let ran = DW_RAN.load(Ordering::Relaxed);
    assert!(ran == 2, "dw test 3: ran {}, expected 2", ran);
    assert!(total == 3, "dw test 3: total {}, expected 3 (1 + 2)", total);
    info!("dw test 3: PASSED (chain ran={}, total={})", ran, total);

    // Test 4: DW mode is cleared after the drain finishes
    assert!(!sched::is_in_dw_mode(),
        "dw test 4: is_in_dw_mode() still true after drain");
    info!("dw test 4: PASSED (is_in_dw_mode = false post-drain)");

    info!("=== run_driver_worker_tests: PASSED ===");
}

#[kmod::test_function(false)]
fn run_i8042_tests() {
    // Wait for PnP to start i8042 + input class device.
    sched::delay_ms(500, false);

    let handle = match io::open_device_handle("ps/2_port0") {
        Ok(h) => h,
        Err(_) => {
            info!("i8042 test: 'ps/2_port0' device not found, skipping");
            return;
        }
    };

    // Test 1: single reader, 3 ASCII characters
    info!("i8042 test 1: press 3 keys to satisfy single read...");
    let mut buf1 = [Keystroke::default(); 3];
    let _ = handle.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: buf1.as_mut_ptr() as usize, size: 3 * size_of::<Keystroke>() },
        offset: 0
    }, false);
    info!("i8042 test 1: got keystrokes: {:?}", buf1);

    // Test 2: three concurrent readers, each opens its own handle.
    info!("i8042 test 2: spawning 3 concurrent readers (need 2+4+1 = 7 more keys)...");
    let t1 = sched::create_thread(i8042_reader_a, core::ptr::null_mut()).expect("i8042: spawn reader-a");
    let t2 = sched::create_thread(i8042_reader_b, core::ptr::null_mut()).expect("i8042: spawn reader-b");
    let t3 = sched::create_thread(i8042_reader_c, core::ptr::null_mut()).expect("i8042: spawn reader-c");
    t1.wait(false); t2.wait(false); t3.wait(false);
}

#[cfg(feature = "kunit-test")]
extern "C" fn i8042_reader_a() -> ! {
    let h = io::open_device_handle("ps/2_port0").expect("i8042 reader-a: no input device");
    let mut buf = [Keystroke::default(); 2];
    info!("i8042 reader-a: waiting for 2 keystrokes");
    let _ = h.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: buf.as_mut_ptr() as usize, size: 2 * size_of::<Keystroke>()},
        offset: 0,
    }, false);
    info!("i8042 reader-a: got chars: {:?}", buf);
    sched::exit_thread(ExitInfo::normal(0))
}

#[cfg(feature = "kunit-test")]
extern "C" fn i8042_reader_b() -> ! {
    let h = io::open_device_handle("ps/2_port0").expect("i8042 reader-b: no input device");
    let mut buf = [Keystroke::default(); 4];
    info!("i8042 reader-b: waiting for 4 keystrokes");
    let _ = h.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: buf.as_mut_ptr() as usize, size: 4 * size_of::<Keystroke>()},
        offset: 0,
    }, false);
    info!("i8042 reader-b: got chars: {:?}", buf);
    sched::exit_thread(ExitInfo::normal(0))
}

#[cfg(feature = "kunit-test")]
extern "C" fn i8042_reader_c() -> ! {
    let h = io::open_device_handle("ps/2_port0").expect("i8042 reader-c: no input device");
    let mut buf = [Keystroke::default(); 1];
    info!("i8042 reader-c: waiting for 1 keystroke");
    let _ = h.read(io::ReadRequest {
        buffer: MemoryRegion { base_address: buf.as_mut_ptr() as usize, size: size_of::<Keystroke>() },
        offset: 0,
    }, false);
    info!("i8042 reader-c: got char: {:?}", buf);
    sched::exit_thread(ExitInfo::normal(0))
}

#[cfg(feature = "kunit-test")]
static REMOVE_RACE_SYNC_NON_SUCCESS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "kunit-test")]
static REMOVE_RACE_ASYNC_REJECTED: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "kunit-test")]
static REMOVE_RACE_ASYNC_NON_SUCCESS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "kunit-test")]
static REMOVE_RACE_ASYNC_COMPLETED: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "kunit-test")]
static REMOVE_RACE_STOP: AtomicBool = AtomicBool::new(false);

// Blocking reader: one outstanding sync read at a time, re-issued in a loop
// until it sees a non-Success result (device stopped/removed underneath it).
#[cfg(feature = "kunit-test")]
extern "C" fn remove_race_sync_reader() -> ! {
    let handle = match io::open_device_handle("ps/2_port0") {
        Ok(h) => h,
        Err(_) => {
            info!("remove_race_sync_reader: device could not be opened");
            sched::exit_thread(ExitInfo::normal(0));
        }
    };

    let mut buf = [Keystroke::default(); 1];
    loop {
        let res = handle.read(io::ReadRequest {
            buffer: MemoryRegion { base_address: buf.as_mut_ptr() as usize, size: size_of::<Keystroke>() },
            offset: 0
        }, false);

        if !matches!(res, Ok(Status::Success)) {
            REMOVE_RACE_SYNC_NON_SUCCESS.fetch_add(1, Ordering::Relaxed);
            sched::exit_thread(ExitInfo::normal(0));
        }
    }
}

#[cfg(feature = "kunit-test")]
extern "C" fn remove_race_async_completion(result: *const IrpResult, ctx: *mut core::ffi::c_void) {
    let status = unsafe { (*result).status };
    REMOVE_RACE_ASYNC_COMPLETED.fetch_add(1, Ordering::Relaxed);
    if status != Status::Success {
        REMOVE_RACE_ASYNC_NON_SUCCESS.fetch_add(1, Ordering::Relaxed);
    }
    unsafe { &*(ctx as *const KSem) }.signal();
}

#[cfg(feature = "kunit-test")]
extern "C" fn remove_race_async_issuer() -> ! {
    let handle = match io::open_device_handle("ps/2_port0") {
        Ok(h) => h,
        Err(_) => {
            info!("remove_race_async_issuer: device could not be opened");
            sched::exit_thread(ExitInfo::normal(0));
        }
    };

    let mut buf = [Keystroke::default(); 1];
    let sem = KSem::new(0, 1);

    while !REMOVE_RACE_STOP.load(Ordering::Acquire) {
        let res = io::io_request_async(
            &handle,
            IrpMajor::Read,
            IrpMinor::None,
            MemoryRegion { base_address: buf.as_mut_ptr() as usize, size: size_of::<Keystroke>() },
            0,
            None,
            remove_race_async_completion,
            &sem as *const KSem as *mut core::ffi::c_void
        );

        match res {
            Err(_) => {
                REMOVE_RACE_ASYNC_REJECTED.fetch_add(1, Ordering::Relaxed);
                // No IRP was allocated, so there's nothing to wait on -- but
                // don't busy-spin re-submitting as fast as possible once the
                // device starts rejecting everything.
                sched::delay_ms(5, false);
            }
            Ok(_) => {
                sem.wait_with_timeout(3000, false)
                    .expect("remove_race_async_issuer: async completion timed out (possible hang/regression)");
            }
        }
    }
    sched::exit_thread(ExitInfo::normal(0));
}

#[kmod::test_function(true)]
fn run_remove_race_tests() {
    let dev_id = match io::open_device_handle("ps/2_port0") {
        Ok(h) => h.id(),
        Err(_) => {
            info!("run_remove_race_tests: SKIP -- ps/2_port0 device missing");
            return;
        }
    };

    REMOVE_RACE_SYNC_NON_SUCCESS.store(0, Ordering::Relaxed);
    REMOVE_RACE_ASYNC_REJECTED.store(0, Ordering::Relaxed);
    REMOVE_RACE_ASYNC_NON_SUCCESS.store(0, Ordering::Relaxed);
    REMOVE_RACE_ASYNC_COMPLETED.store(0, Ordering::Relaxed);
    REMOVE_RACE_STOP.store(false, Ordering::Release);

    const SYNC_READERS: usize = 8;
    const ASYNC_ISSUERS: usize = 6;

    let mut sync_threads = alloc::vec::Vec::with_capacity(SYNC_READERS);
    for _ in 0..SYNC_READERS {
        sync_threads.push(
            sched::create_thread(remove_race_sync_reader, core::ptr::null_mut())
                .expect("Failed to spawn remove_race_sync_reader")
        );
    }
    let mut async_threads = alloc::vec::Vec::with_capacity(ASYNC_ISSUERS);
    for _ in 0..ASYNC_ISSUERS {
        async_threads.push(
            sched::create_thread(remove_race_async_issuer, core::ptr::null_mut())
                .expect("Failed to spawn remove_race_async_issuer")
        );
    }

    // Let a burst of concurrent sync + async reads actually get admitted and
    // start hitting the driver before we remove the device out from under them.
    sched::delay_ms(200, false);

    info!(
        "remove race: removing device {} while {} sync readers + {} async issuers are active",
        dev_id, SYNC_READERS, ASYNC_ISSUERS
    );
    match io::get_device(dev_id) {
        Some(handle) => io::remove_device(&handle),
        None => {
            info!("run_remove_race_tests: SKIP -- device disappeared before remove");
            REMOVE_RACE_STOP.store(true, Ordering::Release);
            for t in sync_threads { t.wait(false); }
            for t in async_threads { t.wait(false); }
            return;
        }
    }

    REMOVE_RACE_STOP.store(true, Ordering::Release);
    for t in sync_threads {
        t.wait(false);
    }
    for t in async_threads {
        t.wait(false);
    }

    let sync_non_success = REMOVE_RACE_SYNC_NON_SUCCESS.load(Ordering::Relaxed);
    let async_rejected = REMOVE_RACE_ASYNC_REJECTED.load(Ordering::Relaxed);
    let async_non_success = REMOVE_RACE_ASYNC_NON_SUCCESS.load(Ordering::Relaxed);
    let async_completed = REMOVE_RACE_ASYNC_COMPLETED.load(Ordering::Relaxed);
    info!(
        "remove race: {} / {} sync readers non-success, {} async submissions rejected, {} async completions ({} non-success)",
        sync_non_success, SYNC_READERS, async_rejected, async_completed, async_non_success
    );
    assert!(sync_non_success == SYNC_READERS, "all pending sync reads must be unblocked (non-success) by device removal");
    assert!(async_rejected > 0, "async submissions issued after removal must be rejected by the state guard");
    assert!(async_completed > 0, "async issuers must have actually gotten at least one request dispatched and completed");

    assert!(io::get_device(dev_id).is_none(), "device must be gone from the registry after remove_device");
    info!("run_remove_race_tests: PASSED");
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

pub fn system_shutdown(restart: bool) -> ! {
    info!("system_shutdown: restart={}", restart);

    fs::stop_fs();
    info!("system_shutdown: fs stopped");

    let dev = io::open_device_handle("acpi");
    if let Ok(device) = dev {
        io::stop_device(device.id(), true);
        io::pnp_fence();
    }

    info!("system_shutdown: devices stopped");

    if restart {
        #[cfg(feature = "acpi")]
        {
            let status = acpi_intf::acpi_reset();
            if status != acpi_intf::AE_OK {
                info!("system_shutdown: AcpiReset failed ({:#X}), falling back to 8042 reset", status);
                
                #[cfg(target_arch = "x86_64")]
                unsafe { kernel_intf::hw::outb(0x64, 0xFE); }
            }
        }
        #[cfg(all(not(feature = "acpi"), target_arch = "x86_64"))]
        unsafe { kernel_intf::hw::outb(0x64, 0xFE); }
    } else {
        #[cfg(feature = "acpi")]
        {
            acpi_intf::acpi_enter_sleep_state_prep(acpi_intf::ACPI_SLEEP_S5);
            let int_status = hal::disable_interrupts();
            acpi_intf::acpi_enter_sleep_state(acpi_intf::ACPI_SLEEP_S5);
            hal::enable_interrupts(int_status);
        }
    }

    info!("system_shutdown: power action did not take effect, halting");
    hal::halt();
}