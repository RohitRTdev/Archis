#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::c_void;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicUsize, Ordering};

use kernel_intf::{
    info,
    Lock,
    create_spinlock, acquire_spinlock, release_spinlock,
    io_complete_irp, io_set_cancel_routine, io_start_processing,
    io_start_device, io_stop_device,
    io_send_request,
};
use kernel_intf::driver::{
    DeviceObject, DriverObject, Irp, IrpMajor, IrpMinor,
    Keystroke, KeystrokeHandler, RegisterHandlerInfo, ReqInfo, Status,
    create_device, create_device_by_id,
};
use kernel_intf::mem::PoolAllocatorGlobal;

// ── Ring sizes ──────────────────────────────────────────────────────────────
const ASCII_BUF_SIZE: usize = 256;
const RAW_BUF_SIZE:   usize = 64;
const MAX_PENDING:    usize = 16;

// ── Global device bookkeeping ────────────────────────────────────────────────

// Number of kbd FDOs created (used to generate "kbd0", "kbd1", … names).
static KBD_COUNT: AtomicUsize = AtomicUsize::new(0);

// Number of port devices currently in the Started state.
static STARTED_PORT_COUNT: AtomicUsize = AtomicUsize::new(0);

// ID of the "input" class device (usize::MAX = not yet created).
static INPUT_DEVICE_ID: AtomicUsize = AtomicUsize::new(usize::MAX);

// Raw pointer to the "input" class DeviceObject.
static INPUT_DEVICE_PTR: AtomicUsize = AtomicUsize::new(0);

// Raw pointer to the InputCtx for the class device.
static INPUT_CTX_PTR: AtomicUsize = AtomicUsize::new(0);

// ── Pending-read bookkeeping ─────────────────────────────────────────────────
#[derive(Clone, Copy)]
struct PendingEntry {
    irp:       *mut Irp,
    // Number of ASCII bytes the caller wants.
    requested: usize,
}

// ── Per-class-device context ─────────────────────────────────────────────────
struct InputCtx {
    lock:       Lock,

    // Raw keystroke ring (currently kept but not exposed via read – reserved
    // for future raw-mode reads).
    raw_ring:   [Keystroke; RAW_BUF_SIZE],
    raw_head:   usize,
    raw_tail:   usize,
    raw_len:    usize,

    // ASCII byte ring — what the userspace read() receives.
    ascii_ring: [u8; ASCII_BUF_SIZE],
    ascii_head: usize,
    ascii_tail: usize,
    ascii_len:  usize,

    pending:    [PendingEntry; MAX_PENDING],
    pending_len: usize,
}

unsafe impl Send for InputCtx {}
unsafe impl Sync for InputCtx {}

impl InputCtx {
    const fn zeroed() -> Self {
        Self {
            lock:       Lock { lock: 0, int_status: false },
            raw_ring:   [Keystroke { scancode: 0, ascii: 0, flags: 0 }; RAW_BUF_SIZE],
            raw_head:   0,
            raw_tail:   0,
            raw_len:    0,
            ascii_ring: [0u8; ASCII_BUF_SIZE],
            ascii_head: 0,
            ascii_tail: 0,
            ascii_len:  0,
            pending:    [PendingEntry { irp: null_mut(), requested: 0 }; MAX_PENDING],
            pending_len: 0,
        }
    }

    fn push_raw(&mut self, ks: Keystroke) {
        if self.raw_len < RAW_BUF_SIZE {
            self.raw_ring[self.raw_tail] = ks;
            self.raw_tail = (self.raw_tail + 1) % RAW_BUF_SIZE;
            self.raw_len += 1;
        }
    }

    fn push_ascii(&mut self, ch: u8) {
        if self.ascii_len < ASCII_BUF_SIZE {
            self.ascii_ring[self.ascii_tail] = ch;
            self.ascii_tail = (self.ascii_tail + 1) % ASCII_BUF_SIZE;
            self.ascii_len += 1;
        }
    }

    unsafe fn dequeue_ascii_into(&mut self, dst: *mut u8, n: usize) {
        for i in 0..n {
            unsafe { dst.add(i).write(self.ascii_ring[self.ascii_head]); }
            self.ascii_head = (self.ascii_head + 1) % ASCII_BUF_SIZE;
            self.ascii_len -= 1;
        }
    }

    fn remove_pending(&mut self, irp: *mut Irp) -> bool {
        for i in 0..self.pending_len {
            if self.pending[i].irp == irp {
                self.pending[i] = self.pending[self.pending_len - 1];
                self.pending_len -= 1;
                return true;
            }
        }
        false
    }
}

// ── Per-port-device context ───────────────────────────────────────────────────
struct KbdCtx {
    // The i8042 FDO (ps/2_portN device) below us in the stack.
    ps2_dev: *const DeviceObject,
}

unsafe impl Send for KbdCtx {}
unsafe impl Sync for KbdCtx {}

// ── No-op keyboard handler (used to deregister from i8042 on stop) ───────────
unsafe extern "C" fn noop_handler(_: *const Keystroke, _: usize, _: *mut c_void) {}

// ── Driver entry ─────────────────────────────────────────────────────────────
#[kmod::init(driver)]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("{} initializing (id={})...", driver.get_name(), driver.id);

    kmod::dispatch_init!(driver, dispatch_add, dispatch_pnp, dispatch_read);

    // Allocate the InputCtx for the class device.
    let ctx = alloc::boxed::Box::new_in(InputCtx::zeroed(), PoolAllocatorGlobal);
    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(ctx).0 as *mut c_void;

    // Create the "input" class device — lifecycle is fully driver-managed.
    let dev = create_device_by_id(driver.id, Some("input"), ctx_ptr, None, true);
    if dev.is_null() {
        info!("input: create_device failed for class device");
        return Status::Failed;
    }

    let dev_id = unsafe { (*dev).id };
    INPUT_DEVICE_ID.store(dev_id, Ordering::Release);
    INPUT_DEVICE_PTR.store(dev as usize, Ordering::Release);
    INPUT_CTX_PTR.store(ctx_ptr as usize, Ordering::Release);

    // Create a spinlock for the InputCtx.
    let ictx = unsafe { &mut *(ctx_ptr as *mut InputCtx) };
    create_spinlock(&mut ictx.lock);

    // The class device stays Stopped until the first port device starts.
    info!("input: class device created id={}", dev_id);
    Status::Success
}

// ── dispatch_add — called once per i8042 port device ─────────────────────────
#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, pdo: Option<&DeviceObject>) -> Status {
    let idx  = KBD_COUNT.fetch_add(1, Ordering::Relaxed);
    let name: &'static str =
        alloc::boxed::Box::leak(alloc::format!("kbd{}", idx).into_boxed_str());

    let kbd_ctx = alloc::boxed::Box::new_in(
        KbdCtx { ps2_dev: pdo.map(|p| p as *const _).unwrap_or(null_mut()) },
        PoolAllocatorGlobal,
    );
    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(kbd_ctx).0 as *mut c_void;

    let dev = create_device(driver, Some(name), ctx_ptr, pdo, false);
    if dev.is_null() {
        info!("input: create_device failed for {}", name);
        unsafe {
            drop(alloc::boxed::Box::from_raw_in(ctx_ptr as *mut KbdCtx, PoolAllocatorGlobal));
        }
        return Status::Failed;
    }

    info!("input: added device '{}'", name);
    Status::Success
}

// ── dispatch_pnp ──────────────────────────────────────────────────────────────
#[kmod::dispatch_handler]
fn dispatch_pnp(device: &DeviceObject, request: &mut Irp) -> Status {
    match request.minor_code {
        IrpMinor::Start  => do_start(device, request),
        IrpMinor::Stop   => do_stop(device, request),
        IrpMinor::Remove => do_remove(device, request),
        _                => Status::Unsupported,
    }
}

fn do_start(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("input: start '{}'", device.get_name().unwrap_or("?"));

    let kbd = unsafe { &*(device.ctx as *mut KbdCtx) };
    let input_ctx_ptr = INPUT_CTX_PTR.load(Ordering::Acquire) as *mut c_void;

    // Register our keystroke_received handler with i8042.
    let req_info = ReqInfo {
        register_handler: RegisterHandlerInfo {
            handler: keystroke_received as KeystrokeHandler,
            context: input_ctx_ptr,
        },
    };
    io_send_request(
        kbd.ps2_dev,
        IrpMajor::Control as usize,
        IrpMinor::RegisterKeyboardHandler as usize,
        0, 0, 0,
        &req_info as *const ReqInfo,
        None,
        null_mut(),
    );

    let prev = STARTED_PORT_COUNT.fetch_add(1, Ordering::AcqRel);
    if prev == 0 {
        // First port device started — bring up the class device.
        let input_dev = INPUT_DEVICE_PTR.load(Ordering::Acquire) as *const DeviceObject;
        io_start_device(input_dev);
        info!("input: class device started");
    }

    request.complete_irp(Status::Success);
    Status::Success
}

fn do_stop(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("input: stop '{}'", device.get_name().unwrap_or("?"));

    let kbd = unsafe { &*(device.ctx as *mut KbdCtx) };

    // Deregister handler first to prevent use-after-free once the InputCtx
    // could be freed — the stack tears down top-down (input before i8042).
    let req_info = ReqInfo {
        register_handler: RegisterHandlerInfo {
            handler: noop_handler as KeystrokeHandler,
            context: null_mut(),
        },
    };
    io_send_request(
        kbd.ps2_dev,
        IrpMajor::Control as usize,
        IrpMinor::RegisterKeyboardHandler as usize,
        0, 0, 0,
        &req_info as *const ReqInfo,
        None,
        null_mut(),
    );

    // Saturating sub avoids underflow if stop arrives out of order.
    let prev = STARTED_PORT_COUNT.fetch_sub(1, Ordering::AcqRel);
    let remaining = prev.saturating_sub(1);
    if remaining == 0 {
        let input_dev = INPUT_DEVICE_PTR.load(Ordering::Acquire) as *const DeviceObject;
        io_stop_device(input_dev);
        info!("input: class device stopped");
        // Class device is NOT removed — it persists until an explicit unload.
    }

    request.complete_irp(Status::Success);
    Status::Success
}

fn do_remove(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("input: remove '{}'", device.get_name().unwrap_or("?"));

    if !device.ctx.is_null() {
        unsafe {
            drop(alloc::boxed::Box::from_raw_in(
                device.ctx as *mut KbdCtx,
                PoolAllocatorGlobal,
            ));
        }
    }

    request.complete_irp(Status::Success);
    Status::Success
}

// ── dispatch_read — ASCII reads from the "input" class device ─────────────────
#[kmod::dispatch_handler]
fn dispatch_read(device: &DeviceObject, request: &mut Irp) -> Status {
    // Only serve reads aimed at the "input" class device.
    let input_id = INPUT_DEVICE_ID.load(Ordering::Acquire);
    if device.id != input_id {
        request.complete_irp(Status::Unsupported);
        return Status::Unsupported;
    }

    let requested = request.buffer.size;
    if requested == 0 {
        request.complete_irp(Status::Success);
        return Status::Success;
    }
    if requested > ASCII_BUF_SIZE {
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    let ctx = unsafe { &mut *(INPUT_CTX_PTR.load(Ordering::Acquire) as *mut InputCtx) };

    acquire_spinlock(&mut ctx.lock);

    if ctx.ascii_len >= requested {
        let dst = request.buffer.base_address as *mut u8;
        unsafe { ctx.dequeue_ascii_into(dst, requested); }
        request.bytes_completed = requested;
        release_spinlock(&mut ctx.lock);
        request.complete_irp(Status::Success);
        return Status::Success;
    }

    if ctx.pending_len == MAX_PENDING {
        release_spinlock(&mut ctx.lock);
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    let irp_ptr = request as *mut Irp;
    ctx.pending[ctx.pending_len] = PendingEntry { irp: irp_ptr, requested };
    ctx.pending_len += 1;
    release_spinlock(&mut ctx.lock);

    io_set_cancel_routine(request, input_cancel);

    Status::Pending
}

extern "C" fn input_cancel(dev: *const DeviceObject, irp: *mut Irp) {
    // Only the "input" class device queues pending reads.
    let input_id = INPUT_DEVICE_ID.load(Ordering::Acquire);
    if !dev.is_null() && unsafe { (*dev).id } == input_id {
        let ctx = unsafe { &mut *(INPUT_CTX_PTR.load(Ordering::Acquire) as *mut InputCtx) };
        acquire_spinlock(&mut ctx.lock);
        ctx.remove_pending(irp);
        release_spinlock(&mut ctx.lock);
    }
    io_complete_irp(irp, Status::Cancelled);
}

// ── Keystroke handler — called by keyboard_dw in the i8042 driver ─────────────
//
// Batch pattern: push ALL new keystrokes first (tail advances), then satisfy
// all pending IRPs (head advances per IRP), all under a single lock window.
unsafe extern "C" fn keystroke_received(
    keystrokes: *const Keystroke,
    count:      usize,
    context:    *mut c_void,
) {
    let ctx = unsafe { &mut *(context as *mut InputCtx) };

    acquire_spinlock(&mut ctx.lock);

    // 1. Push all new keystrokes into both rings.
    for i in 0..count {
        let ks = unsafe { &*keystrokes.add(i) };
        ctx.push_raw(*ks);
        // Only key-press events with a printable ASCII char go into the ASCII ring.
        if ks.ascii != 0 && ks.flags & 1 == 0 {
            ctx.push_ascii(ks.ascii);
        }
    }

    // 2. Collect every pending IRP that now has enough data.
    let mut collected     = [null_mut::<Irp>(); MAX_PENDING];
    let mut collected_len = 0;
    let mut i = 0;

    while i < ctx.pending_len {
        let entry = ctx.pending[i];
        if ctx.ascii_len >= entry.requested {
            let dst = unsafe { (*entry.irp).buffer.base_address as *mut u8 };
            unsafe { ctx.dequeue_ascii_into(dst, entry.requested); }
            unsafe { (*entry.irp).bytes_completed = entry.requested; }
            ctx.pending[i] = ctx.pending[ctx.pending_len - 1];
            ctx.pending_len -= 1;
            collected[collected_len] = entry.irp;
            collected_len += 1;
        } else {
            i += 1;
        }
    }

    release_spinlock(&mut ctx.lock);

    // 3. Complete collected IRPs outside the lock.
    for k in 0..collected_len {
        let irp = collected[k];
        if io_start_processing(irp) {
            io_complete_irp(irp, Status::Success);
        }
    }
}
