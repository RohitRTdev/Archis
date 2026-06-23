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

#[cfg(feature = "acpi")]
use acpi_intf::{ACPI_SLEEP_S5, acpi_enter_sleep_state, acpi_enter_sleep_state_prep};

#[cfg(feature = "kunit-test")] 
use {
    core::sync::atomic::{AtomicUsize, AtomicBool, Ordering},
    core::ffi::c_void,
    io::OpenDeviceHandle
};

use kernel_intf::{info, debug};
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
use kernel_intf::driver::{IrpMajor, IrpMinor, IrpResult, Keystroke, create_device_by_id};
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

struct InitFS {
    fs: BTreeMap<&'static str, &'static [u8]>,
    symlinks: BTreeMap<&'static str, &'static str>
}

static INIT_FS: Once<InitFS> = Once::new();  
static REMAP_LIST: Spinlock<FixedList<RemapEntry, {Region2 as usize}>> = Spinlock::new(List::new());
//#[cfg(feature = "kunit-test")]
static THREAD_DONE_SEM: Once<crate::sync::KSem> = Once::new();

// Simple worker used in run_proc_thread_tests test 3.
#[cfg(feature = "kunit-test")]
extern "C" fn test_thread_runner() -> ! {
    let id = sched::get_current_task_id().unwrap_or(0);
    info!("test_thread_runner: started (id={})", id);
    sched::delay_ms(500, false);
    info!("test_thread_runner: signaling done (id={})", id);
    THREAD_DONE_SEM.get().unwrap().signal();
    sched::exit_thread(0);
}

// Tests:
//   1. Single kernel process create + wait + exit-code check.
//   2. Three concurrent kernel processes, each waited individually.
//   3. Three worker threads synchronised through a semaphore.
#[kmod::test_function(true)]
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
        &mut ctx as *mut TestCtx as *mut core::ffi::c_void,
        false,
        false
    )
    .expect("proc/thread test 1: failed to create process");

    proc1.wait(false);
    let code1 = proc1.lock().get_exit_code();
    info!("proc/thread test 1: process exited with code {}", code1);
    info!("proc/thread test 1: ctx.val1 after = {} (expect 10)", ctx.val1);

    // Test 2: three concurrent processes (no context), wait for all
    info!("--- proc/thread test 2: concurrent processes ---");

    const PROC_COUNT: usize = 3;
    let mut procs = alloc::vec::Vec::with_capacity(PROC_COUNT);
    for i in 0..PROC_COUNT {
        let p = sched::create_process(
            &["libtest1.so", alloc::format!("concurrent_proc_{}", i).as_str()],
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
        info!("proc/thread test 2: process {} exited with code {}", i, p.lock().get_exit_code());
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
            sched::exit_thread(0);
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
            sched::exit_thread(0);
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
    sched::exit_thread(0);
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
    sched::kill_thread(tid, 0);
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
    let dup = create_device_by_id(driver_id, Some("input"), core::ptr::null_mut(), None, false);
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
    sched::exit_thread(0);
}

#[cfg(feature = "kunit-test")]
extern "C" fn state_start_once() -> ! {
    let dev = state_test_handle();
    let r = dev.start();
    info!("state_start_once (tid={}): start -> {:?}",
          sched::get_current_task_id().unwrap_or(0),
          r.map(|s| s as isize));
    sched::exit_thread(0);
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
    sched::exit_thread(0);
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
    dev.stop().expect("stop must succeed");
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
            sched::exit_thread(0);
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
            sched::exit_thread(0);
        }
        extern "C" fn t6_short() -> ! {
            let res = T6_SEM.get().unwrap().wait_with_timeout(200, false).is_ok();
            if res { SYNC_SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed); }
            else   { SYNC_TIMEOUT_COUNT.fetch_add(1, Ordering::Relaxed); }
            SYNC_WAKE_COUNT.fetch_add(1, Ordering::Relaxed);
            SYNC_TEST_DONE.get().unwrap().signal();
            sched::exit_thread(0);
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
            sched::exit_thread(0);
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
            sched::exit_thread(0);
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
            sched::exit_thread(0);
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
            sched::exit_thread(0);
        }
        extern "C" fn t11_signaller() -> ! {
            for _ in 0..N {
                T11_SEM.get().unwrap().signal();
            }
            sched::exit_thread(0);
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

// === User loader & userspace tests ===
//
// Test 1: basic load — single cat process (exercises ELF parsing,
//         dependency loading of libc.so, relocations, all syscalls).
//         spawns tester process internally.
// Test 2: warm load — two concurrent cat processes share the
//         read-only pages of libc.so.
// Test 3: shared dep — cat and ls launched concurrently,
//         both pull in libc.so (warm-loads the shared dependency).
// Test 4: ls test — spawn ls internally only.
#[kmod::test_function(false)]
fn run_user_tests() {
    // Test 1: basic user process with dependency
    info!("--- user test 1: basic load (cat) ---");
    let p1 = sched::create_process(
        &["cat"],
        core::ptr::null_mut(),
        true,
        false
    ).expect("user test 1: failed to create process");
    p1.wait(false);
    let code1 = p1.lock().get_exit_code();
    info!("user test 1: cat exited with code {}", code1);

    // Test 2: warm load — same image in two concurrent processes
    info!("--- user test 2: warm load (2x cat) ---");
    let p2a = sched::create_process(
        &["cat"],
        core::ptr::null_mut(),
        true,
        false
    ).expect("user test 2: failed to create process A");
    let p2b = sched::create_process(
        &["cat"],
        core::ptr::null_mut(),
        true,
        false
    ).expect("user test 2: failed to create process B");
    p2a.wait(false);
    p2b.wait(false);
    info!(
        "user test 2: process A exited with code {}, B with code {}",
        p2a.lock().get_exit_code(),
        p2b.lock().get_exit_code()
    );

    drop(p1);drop(p2a);drop(p2b);
    // Test 3: different images, shared dependency
    info!("--- user test 3: shared dep (cat + ls) ---");
    let p3a = sched::create_process(
        &["cat"],
        core::ptr::null_mut(),
        true,
        false
    ).expect("user test 3: failed to create cat process");
    let p3b = sched::create_process(
        &["ls"],
        core::ptr::null_mut(),
        true,
        false
    ).expect("user test 3: failed to create ls process");
    p3a.wait(false);
    p3b.wait(false);
    info!(
        "user test 3: cat exited with code {}, cat with code {}",
        p3a.lock().get_exit_code(),
        p3b.lock().get_exit_code()
    );

    // Test 4: user-initiated process spawn
    info!("--- user test 4: ls test ---");
    let p4 = sched::create_process(
        &["ls"],
        core::ptr::null_mut(),
        true,
        false
    ).expect("user test 4: failed to create ls process");
    p4.wait(false);
    info!("user test 4: ls exited with code {}", p4.lock().get_exit_code());

    info!("=== run_user_tests: PASSED ===");
}

#[kmod::test_function(false)]
fn run_signal_tests() {
    static SEM: Once<KEvent> = Once::new();
    static PID: AtomicUsize = AtomicUsize::new(0);
    SEM.call_once(|| {
        KEvent::new(false)
    });

    extern "C" fn signaller() -> ! {
        let _ = SEM.get().unwrap().wait(false);
        let pid = PID.load(Ordering::Acquire); 
        info!("Issuing signal from child thread");
        sched::issue_signal(pid, SIGKILL);

        sched::exit_thread(0);
    }

    sched::create_thread(signaller, core::ptr::null_mut()).unwrap();
    info!("--- signal test 1: user signal handler via sigreturn ---");
    let p1 = sched::create_process(
        &["signal_test"],
        core::ptr::null_mut(),
        true,
        false
    ).expect("signal test 1: failed to create signal_test process");



    let pid = p1.lock().get_id();
    info!("signal test 1: created process pid={}", pid);

    sched::delay_ms(1000, false);
    PID.store(pid, Ordering::Release);
    SEM.get().unwrap().signal();

    info!("signal test 1: issuing signals to pid={}", pid);
    sched::issue_signal(pid, sched::SIGSEGV);
    sched::issue_signal(pid, sched::SIGILL);

    p1.wait(false);
    info!("signal test 1: process exited with code {}", p1.lock().get_exit_code());

    info!("=== run_signal_tests: END ===");
}

fn kern_main() -> ! {
    info!("Starting main kernel init");

    mem::reclaim_pages();

    sched::init();
    loader::init();
    io::init();

#[cfg(feature = "acpi")]
    acpica::init();
    kernel_intf::run_tests!();
    info!("Main task going to sleep");
    info!("====TTY mode====");
    let kbd = crate::io::open_device_handle("input").expect("Failed to open input device!");
    let input_buf: [u8; 256] = [0; 256];
    loop {
        kbd.read(crate::io::ReadRequest{
            buffer: MemoryRegion {
                base_address: input_buf.as_ptr().addr(),
                size: 9
            },
            offset: 0
        }, false).expect("Failed to read input device!");

        let command = unsafe {
            let command_slice = core::slice::from_raw_parts(input_buf.as_ptr(), 9);
            core::str::from_utf8(command_slice).expect("Failed to decode utf-8")
        };

        if command == "shutdown\n" {
            #[cfg(feature = "acpi")]
            {
                acpi_enter_sleep_state_prep(ACPI_SLEEP_S5);
                acpi_enter_sleep_state(ACPI_SLEEP_S5);
            }
        }
        else {
            kernel_intf::println!();
            kernel_intf::print!("Type shutdown:");
        }
    }
    //hal::sleep();
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
    crate::sched::SIGKILL,
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
    sched::exit_thread(0)
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
    sched::exit_thread(0)
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