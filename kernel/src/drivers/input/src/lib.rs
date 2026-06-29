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
    io_send_request
};
use kernel_intf::ds::RingBuffer;
use kernel_intf::driver::{
    DeviceObject, DriverObject, Irp, IrpMajor, IrpMinor,
    Keystroke, KeystrokeHandler, RegisterHandlerInfo, ReqInfo, Status,
    create_device, create_device_by_id
};
use kernel_intf::mem::PoolAllocatorGlobal;

const ASCII_BUF_SIZE: usize = 256;
const RAW_BUF_SIZE:   usize = 64;
const MAX_PENDING:    usize = 16;


static KBD_COUNT: AtomicUsize = AtomicUsize::new(0);
static STARTED_PORT_COUNT: AtomicUsize = AtomicUsize::new(0);
static INPUT_DEVICE_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static INPUT_DEVICE_PTR: AtomicUsize = AtomicUsize::new(0);
static INPUT_CTX_PTR: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy)]
struct PendingEntry {
    irp:       *mut Irp,
    // Number of ASCII bytes the caller wants.
    requested: usize
}

struct InputCtx {
    lock:       Lock,
    raw_ring:   RingBuffer<Keystroke, RAW_BUF_SIZE>,
    ascii_ring: RingBuffer<u8, ASCII_BUF_SIZE>,
    pending:    [PendingEntry; MAX_PENDING],
    pending_len: usize
}

unsafe impl Send for InputCtx {}
unsafe impl Sync for InputCtx {}

impl InputCtx {
    const fn zeroed() -> Self {
        Self {
            lock:       Lock { lock: 0, int_status: false },
            raw_ring:   RingBuffer::new(Keystroke { scancode: 0, ascii: 0, flags: 0 }),
            ascii_ring: RingBuffer::new(0u8),
            pending:    [PendingEntry { irp: null_mut(), requested: 0 }; MAX_PENDING],
            pending_len: 0
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

struct KbdCtx {
    ps2_dev: *const DeviceObject
}

unsafe impl Send for KbdCtx {}
unsafe impl Sync for KbdCtx {}

unsafe extern "C" fn noop_handler(_: *const Keystroke, _: usize, _: *mut c_void) {}

#[kmod::init(driver)]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("{} initializing (id={})...", driver.get_name(), driver.id);

    kmod::dispatch_init!(driver, dispatch_close, dispatch_open, dispatch_add, dispatch_pnp, dispatch_read);

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

    let ictx = unsafe { &mut *(ctx_ptr as *mut InputCtx) };
    create_spinlock(&mut ictx.lock);

    // The class device stays Stopped until the first port device starts.
    info!("input: class device created id={}", dev_id);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_open(_device: &DeviceObject, req: &mut Irp) -> Status {
    info!("Received open request");
    req.complete_irp(Status::Success);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_close(_device: &DeviceObject, req: &mut Irp) -> Status {
    info!("Received close request");
    req.complete_irp(Status::Success);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, pdo: &DeviceObject) -> Status {
    let idx  = KBD_COUNT.fetch_add(1, Ordering::Relaxed);
    let name: &'static str = alloc::boxed::Box::leak(alloc::format!("kbd{}", idx).into_boxed_str());

    let kbd_ctx = alloc::boxed::Box::new_in(
        KbdCtx { ps2_dev: pdo as *const _ },
        PoolAllocatorGlobal
    );
    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(kbd_ctx).0 as *mut c_void;

    let dev = create_device(driver, Some(name), ctx_ptr, Some(pdo), false);
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

    // Register our keystroke_received handler with port driver
    let req_info = ReqInfo {
        register_handler: RegisterHandlerInfo {
            handler: keystroke_received as KeystrokeHandler,
            context: input_ctx_ptr
        }
    };
    io_send_request(
        kbd.ps2_dev,
        IrpMajor::Control as usize,
        IrpMinor::RegisterKeyboardHandler as usize,
        0, 0, 0,
        &req_info as *const ReqInfo,
        None,
        null_mut()
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
            context: null_mut()
        }
    };
    io_send_request(
        kbd.ps2_dev,
        IrpMajor::Control as usize,
        IrpMinor::RegisterKeyboardHandler as usize,
        0, 0, 0,
        &req_info as *const ReqInfo,
        None,
        null_mut()
    );

    let prev = STARTED_PORT_COUNT.fetch_sub(1, Ordering::AcqRel);
    let remaining = prev.saturating_sub(1);
    if remaining == 0 {
        let input_dev = INPUT_DEVICE_PTR.load(Ordering::Acquire) as *const DeviceObject;
        io_stop_device(input_dev);
        info!("input: class device stopped");
    }

    request.complete_irp(Status::Success);
    Status::Success
}

fn do_remove(device: &DeviceObject, request: &mut Irp) -> Status {
    if let Some(name) = device.get_name() {
        unsafe { drop(alloc::boxed::Box::from_raw(name as *const str as *mut str)); }
    }

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
        info!("Requested bytes more than max buf size={}", ASCII_BUF_SIZE);
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    let ctx = unsafe { &mut *(INPUT_CTX_PTR.load(Ordering::Acquire) as *mut InputCtx) };

    acquire_spinlock(&mut ctx.lock);

    let avail = ctx.ascii_ring.len();
    if avail > 0 {
        let give = avail.min(requested);
        let dst = unsafe { (request.buffer.base_address as *mut u8).add(request.offset) };
        unsafe { ctx.ascii_ring.dequeue_into(dst, give); }
        request.bytes_completed = give;
        release_spinlock(&mut ctx.lock);
        request.complete_irp(Status::Success);
        return Status::Success;
    }

    if ctx.pending_len == MAX_PENDING {
        release_spinlock(&mut ctx.lock);
        info!("Too many irq's pending. Cancelling this one...");
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

unsafe extern "C" fn keystroke_received(
    keystrokes: *const Keystroke,
    count:      usize,
    context:    *mut c_void,
) {
    let ctx = unsafe { &mut *(context as *mut InputCtx) };

    acquire_spinlock(&mut ctx.lock);

    let mut ascii_batch     = [0u8; ASCII_BUF_SIZE];
    let mut ascii_batch_len = 0usize;

    for i in 0..count {
        let ks = unsafe { &*keystrokes.add(i) };
        ctx.raw_ring.push(*ks);
        if ks.ascii != 0 && ks.flags & 1 == 0 {
            ctx.ascii_ring.push(ks.ascii);
            if ascii_batch_len < ASCII_BUF_SIZE {
                ascii_batch[ascii_batch_len] = ks.ascii;
                ascii_batch_len += 1;
            }
        }
    }

    let mut satisfied = 0usize;
    let mut collected = [null_mut::<Irp>(); MAX_PENDING];
    let mut collected_len = 0;

    while ctx.ascii_ring.len() > 0 && satisfied < ctx.pending_len {
        let entry = ctx.pending[satisfied];
        let give = ctx.ascii_ring.len().min(entry.requested);
        let dst = unsafe { ((*entry.irp).buffer.base_address as *mut u8).add((*entry.irp).offset) };
        unsafe { ctx.ascii_ring.dequeue_into(dst, give); }
        unsafe { (*entry.irp).bytes_completed = give; }
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

    if ascii_batch_len > 0 {
        unsafe { tty::tty_input(ascii_batch.as_ptr(), ascii_batch_len); }
    }

    // Complete collected IRPs outside the lock.
    for k in 0..collected_len {
        let irp = collected[k];
        if io_start_processing(irp) {
            io_complete_irp(irp, Status::Success);
        }
    }
}

#[kmod::driver_unload]
fn destroy(driver: &mut DriverObject) {
    info!("Destroying driver {}", driver.get_name());
}
