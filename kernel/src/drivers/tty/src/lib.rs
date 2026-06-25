#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::c_void;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kernel_intf::{
    Lock, ProcessGroupType, SIGINT, SIGTTIN, SessionType, acquire_spinlock, create_spinlock, disable_tty_mode, enable_tty_mode, info, io_complete_irp, io_set_cancel_routine, io_start_processing, proc_drop_pgrp, proc_drop_session, proc_get_pgrp, proc_get_session, proc_is_foreground_pgrp, proc_is_pgrp_active, proc_is_session_active, proc_is_session_leader, proc_issue_pgrp, release_spinlock, sched_get_current_pid, tty_print
};
use kernel_intf::ds::RingBuffer;
use kernel_intf::driver::{
    DeviceObject, DriverObject, Irp, IrpMinor, Status,
    TtyControlInfo, create_device
};
use kernel_intf::mem::PoolAllocatorGlobal;

const INPUT_BUF_SIZE: usize = 256;
const MAX_PENDING:    usize = 16;
const CTRL_C:         u8    = 0x1b;

static TTY_CREATED: AtomicUsize = AtomicUsize::new(0);
static TTY_CTX_PTR: AtomicUsize = AtomicUsize::new(0);
static TTY_CTX_LOCK: AtomicBool = AtomicBool::new(true);

#[derive(Clone, Copy)]
struct PendingEntry {
    irp:       *mut Irp,
    requested: usize
}

struct TtyJobInfo {
    session: SessionType,
    pgrp:    ProcessGroupType
}

struct TtyCtx {
    lock:        Lock,
    input_ring:  RingBuffer<u8, INPUT_BUF_SIZE>,
    pending:     [PendingEntry; MAX_PENDING],
    pending_len: usize,
    enabled:     bool,
    mode:        u8,
    job:         TtyJobInfo
}

unsafe impl Send for TtyCtx {}
unsafe impl Sync for TtyCtx {}

const LINE_BUFFERED: u8 = 1 << 1;
const ECHO: u8 = 1 << 0;

impl TtyCtx {
    const fn zeroed() -> Self {
        Self {
            lock:        Lock { lock: 0, int_status: false },
            input_ring:  RingBuffer::new(0u8),
            pending:     [PendingEntry { irp: null_mut(), requested: 0 }; MAX_PENDING],
            pending_len: 0,
            enabled:     false,
            mode:        LINE_BUFFERED | ECHO,
            job:         TtyJobInfo { session: 0, pgrp: 0 }
        }
    }

    fn remove_pending(&mut self, irp: *mut Irp) -> bool {
        for i in 0..self.pending_len {
            if self.pending[i].irp == irp {
                // Shift remaining irps to left to avoid destroying the fifo order
                for j in i..self.pending_len - 1 {
                    self.pending[j] = self.pending[j + 1];
                }
                self.pending_len -= 1;
                return true;
            }
        }
        false
    }
}

#[kmod::init(driver)]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("{} initializing (id={})...", driver.get_name(), driver.id);
    kmod::dispatch_init!(driver, dispatch_open, dispatch_close, dispatch_add, dispatch_pnp, dispatch_read, dispatch_write, dispatch_control);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, pdo: Option<&DeviceObject>) -> Status {
    if TTY_CREATED.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
        info!("tty: only one device allowed");
        return Status::Failed;
    }

    let ctx = alloc::boxed::Box::new_in(TtyCtx::zeroed(), PoolAllocatorGlobal);
    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(ctx).0 as *mut c_void;

    unsafe { create_spinlock(&mut (*(ctx_ptr as *mut TtyCtx)).lock); }

    let dev = create_device(driver, Some("tty"), ctx_ptr, pdo, false);
    if dev.is_null() {
        info!("tty: create_device failed");
        unsafe {
            drop(alloc::boxed::Box::from_raw_in(ctx_ptr as *mut TtyCtx, PoolAllocatorGlobal));
        }
        TTY_CREATED.store(0, Ordering::Release);
        return Status::Failed;
    }

    TTY_CTX_PTR.store(ctx_ptr as usize, Ordering::Release);
    info!("tty: device created");
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_pnp(device: &DeviceObject, request: &mut Irp) -> Status {
    match request.minor_code {
        IrpMinor::Start  => do_start(request),
        IrpMinor::Stop   => do_stop(device, request),
        IrpMinor::Remove => do_remove(device, request),
        _                => Status::Unsupported
    }
}

fn do_start(request: &mut Irp) -> Status {
    info!("tty: start");
    request.complete_irp(Status::Success);
    Status::Success
}

fn do_stop(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("tty: stop");
    disable_and_fail_pending(device);
    request.complete_irp(Status::Success);
    Status::Success
}

fn do_remove(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("tty: remove");
    disable_and_fail_pending(device);

    TTY_CTX_PTR.store(0, Ordering::Release);

    if !device.ctx.is_null() {
        unsafe {
            let ctx = &mut *(device.ctx as *mut TtyCtx);
            if ctx.job.session != 0 {
                proc_drop_session(ctx.job.session);
                ctx.job.session = 0;
            }
            if ctx.job.pgrp != 0 {
                proc_drop_pgrp(ctx.job.pgrp);
                ctx.job.pgrp = 0;
            }
            drop(alloc::boxed::Box::from_raw_in(
                device.ctx as *mut TtyCtx,
                PoolAllocatorGlobal
            ));
        }
    }

    request.complete_irp(Status::Success);
    Status::Success
}

fn disable_and_fail_pending(device: &DeviceObject) {
    if device.ctx.is_null() {
        return;
    }
    let ctx = unsafe { &mut *(device.ctx as *mut TtyCtx) };

    acquire_spinlock(&mut ctx.lock);
    let to_fail_len = ctx.pending_len;
    let mut to_fail = [null_mut::<Irp>(); MAX_PENDING];
    for i in 0..to_fail_len {
        to_fail[i] = ctx.pending[i].irp;
    }
    ctx.pending_len = 0;
    let was_enabled = ctx.enabled;
    ctx.enabled = false;
    release_spinlock(&mut ctx.lock);

    if was_enabled {
        disable_tty_mode();
    }

    for i in 0..to_fail_len {
        io_complete_irp(to_fail[i], Status::Failed);
    }
}

#[kmod::dispatch_handler]
fn dispatch_open(device: &DeviceObject, request: &mut Irp) -> Status {
    if !device.ctx.is_null() {
        let ctx = unsafe { &mut *(device.ctx as *mut TtyCtx) };
        acquire_spinlock(&mut ctx.lock);
        let already_enabled = ctx.enabled;
        if !already_enabled {
            ctx.enabled = true;
        }
        release_spinlock(&mut ctx.lock);
        if !already_enabled {
            enable_tty_mode();
        }
    }
    request.complete_irp(Status::Success);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_close(device: &DeviceObject, request: &mut Irp) -> Status {
    if !device.ctx.is_null() {
        let ctx = unsafe { &mut *(device.ctx as *mut TtyCtx) };
        acquire_spinlock(&mut ctx.lock);
        let was_enabled = ctx.enabled;
        if was_enabled {
            ctx.enabled = false;
        }
        release_spinlock(&mut ctx.lock);
        if was_enabled {
            disable_tty_mode();
        }
    }
    request.complete_irp(Status::Success);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_read(device: &DeviceObject, request: &mut Irp) -> Status {
    let requested = request.buffer.size;
    if requested == 0 {
        request.complete_irp(Status::Success);
        return Status::Success;
    }

    let ctx = unsafe { &mut *(device.ctx as *mut TtyCtx) };

    let pid = sched_get_current_pid();
    if pid == -1 {
        // Idle task
        info!("Received read request from idle task!");
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }
    let pid = pid as usize;
    let fgrp = {
        acquire_spinlock(&mut ctx.lock);
        let p = ctx.job.pgrp;
        release_spinlock(&mut ctx.lock);
        p
    };

    // Only the current foreground process group can read/write to tty (if it exists)
    if fgrp != 0 && pid != 0 && !proc_is_foreground_pgrp(pid, fgrp) {
        let caller_pgrp = proc_get_pgrp(pid);
        if caller_pgrp != 0 {
            info!("Not foreground process group!");
            proc_issue_pgrp(caller_pgrp, SIGTTIN);
            proc_drop_pgrp(caller_pgrp);
        }
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    acquire_spinlock(&mut ctx.lock);

    let avail = ctx.input_ring.len();
    if avail > 0 {
        let give = avail.min(requested);
        let dst = request.buffer.base_address as *mut u8;
        unsafe { ctx.input_ring.dequeue_into(dst, give); }
        request.bytes_completed = give;
        release_spinlock(&mut ctx.lock);
        request.complete_irp(Status::Success);
        return Status::Success;
    }

    if ctx.pending_len == MAX_PENDING {
        release_spinlock(&mut ctx.lock);
        info!("tty: too many pending reads");
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    let irp_ptr = request as *mut Irp;
    ctx.pending[ctx.pending_len] = PendingEntry { irp: irp_ptr, requested };
    ctx.pending_len += 1;
    release_spinlock(&mut ctx.lock);

    io_set_cancel_routine(request, tty_cancel);
    Status::Pending
}

#[kmod::dispatch_handler]
fn dispatch_write(device: &DeviceObject, request: &mut Irp) -> Status {
    let ctx = unsafe { &mut *(device.ctx as *mut TtyCtx) };
    let pid = sched_get_current_pid();
    if pid == -1 {
        // Idle task
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    let pid = pid as usize;

    let fgrp = {
        acquire_spinlock(&mut ctx.lock);
        let p = ctx.job.pgrp;
        release_spinlock(&mut ctx.lock);
        p
    };
    if fgrp != 0 && pid != 0 && !proc_is_foreground_pgrp(pid, fgrp) {
        let caller_pgrp = proc_get_pgrp(pid);
        if caller_pgrp != 0 {
            info!("Not foreground process group");
            proc_issue_pgrp(caller_pgrp, SIGTTIN);
            proc_drop_pgrp(caller_pgrp);
        }
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    let size = request.buffer.size;
    if size > 0 {
        let bytes = unsafe {
            core::slice::from_raw_parts(request.buffer.base_address as *const u8, size)
        };
        if let Ok(s) = core::str::from_utf8(bytes) {
            tty_print(s);
        }
    }
    request.bytes_completed = size;
    request.complete_irp(Status::Success);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_control(device: &DeviceObject, request: &mut Irp) -> Status {
    let info: TtyControlInfo = unsafe { request.req_info.tty_control };
    let ctx = unsafe { &mut *(device.ctx as *mut TtyCtx) };

    match request.minor_code {
        IrpMinor::SetForegroundPgrp => {
            // Check caller is session leader of TTY's session (if session active)
            let session = {
                acquire_spinlock(&mut ctx.lock);
                let s = ctx.job.session;
                release_spinlock(&mut ctx.lock);
                s
            };
            if session != 0 && proc_is_session_active(session) {
                if !proc_is_session_leader(info.pid, session) {
                    request.complete_irp(Status::Failed);
                    return Status::Failed;
                }
            }

            let new_pgrp = if info.value != 0 { proc_get_pgrp(info.value) } else { 0 };

            acquire_spinlock(&mut ctx.lock);
            let old_pgrp = ctx.job.pgrp;
            ctx.job.pgrp = new_pgrp;
            release_spinlock(&mut ctx.lock);

            if old_pgrp != 0 {
                proc_drop_pgrp(old_pgrp);
            }

            request.complete_irp(Status::Success);
            Status::Success
        }
        IrpMinor::SetControllingTty => {
            let caller_session = proc_get_session(info.pid);
            if caller_session == 0 {
                request.complete_irp(Status::Failed);
                return Status::Failed;
            }
            if !proc_is_session_leader(info.pid, caller_session) {
                proc_drop_session(caller_session);
                request.complete_irp(Status::Failed);
                return Status::Failed;
            }

            if info.value == 1 {
                acquire_spinlock(&mut ctx.lock);
                if ctx.job.session == 0 {
                    // Transfer Arc in — do not drop caller_session
                    ctx.job.session = caller_session;
                    release_spinlock(&mut ctx.lock);
                } else if proc_is_session_leader(info.pid, ctx.job.session) {
                    // Caller already owns this TTY
                    release_spinlock(&mut ctx.lock);
                    proc_drop_session(caller_session);
                } else {
                    info!("tty already owned by another session!");
                    release_spinlock(&mut ctx.lock);
                    proc_drop_session(caller_session);
                    request.complete_irp(Status::Failed);
                    return Status::Failed;
                }
            } else {
                acquire_spinlock(&mut ctx.lock);
                let old_session = ctx.job.session;
                let old_pgrp = ctx.job.pgrp;
                if old_session != 0 && proc_is_session_leader(info.pid, old_session) {
                    ctx.job.session = 0;
                    ctx.job.pgrp = 0;
                    release_spinlock(&mut ctx.lock);
                    proc_drop_session(old_session);
                    if old_pgrp != 0 {
                        proc_drop_pgrp(old_pgrp);
                    }
                } else {
                    info!("Only owning session leader can unset the controlling tty!");
                    release_spinlock(&mut ctx.lock);
                }
                proc_drop_session(caller_session);
            }

            request.complete_irp(Status::Success);
            Status::Success
        }
        _ => {
            request.complete_irp(Status::Unsupported);
            Status::Unsupported
        }
    }
}

extern "C" fn tty_cancel(dev: *const DeviceObject, irp: *mut Irp) {
    if !dev.is_null() {
        let ctx = unsafe { &mut *((*dev).ctx as *mut TtyCtx) };
        acquire_spinlock(&mut ctx.lock);
        ctx.remove_pending(irp);
        release_spinlock(&mut ctx.lock);
    }
    io_complete_irp(irp, Status::Cancelled);
}

#[kmod::export]
fn tty_input(bytes: *const u8, count: usize) {
    while !TTY_CTX_LOCK.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    
    let ctx_ptr = TTY_CTX_PTR.load(Ordering::Acquire);
    if ctx_ptr == 0 || count == 0 {
        return;
    }
    let ctx = unsafe { &mut *(ctx_ptr as *mut TtyCtx) };

    let slice = unsafe { core::slice::from_raw_parts(bytes, count) };
    match str::from_utf8(slice) {
        Ok(utf_str) => {
            tty_print(utf_str);
        },
        _ => {}
    }

    acquire_spinlock(&mut ctx.lock);

    for &b in slice {
        if b == CTRL_C {
            // Skip this byte stream processing and issue signal to foreground pgrp
            let pgrp = ctx.job.pgrp;
            release_spinlock(&mut ctx.lock);
            if pgrp != 0 && proc_is_pgrp_active(pgrp) {
                proc_issue_pgrp(pgrp, SIGINT);
            }
            return;
        }
        ctx.input_ring.push(b);
    }

    let mut collected     = [null_mut::<Irp>(); MAX_PENDING];
    let mut collected_len = 0usize;
    let mut satisfied     = 0usize;

    // Complete as many requests as possible whilst obeying the fifo order
    while ctx.input_ring.len() > 0 && satisfied < ctx.pending_len {
        let entry = ctx.pending[satisfied];
        let give  = ctx.input_ring.len().min(entry.requested);
        let dst   = unsafe { (*entry.irp).buffer.base_address as *mut u8 };
        unsafe {
            ctx.input_ring.dequeue_into(dst, give);
            (*entry.irp).bytes_completed = give;
        }
        collected[collected_len] = entry.irp;
        collected_len += 1;
        satisfied += 1;
    }

    let remaining = ctx.pending_len - satisfied;
    for i in 0..remaining {
        ctx.pending[i] = ctx.pending[satisfied + i];
    }
    ctx.pending_len = remaining;

    release_spinlock(&mut ctx.lock);

    for i in 0..collected_len {
        let irp = collected[i];
        if io_start_processing(irp) {
            io_complete_irp(irp, Status::Success);
        }
    }
}

#[kmod::driver_unload]
fn destroy(driver: &mut DriverObject) {
    info!("Destroying {} driver", driver.get_name());
    TTY_CTX_LOCK.store(false, Ordering::Release);
    TTY_CREATED.store(0, Ordering::Release);
    TTY_CTX_PTR.store(0, Ordering::Release);
    TTY_CTX_LOCK.store(true, Ordering::Release);
}