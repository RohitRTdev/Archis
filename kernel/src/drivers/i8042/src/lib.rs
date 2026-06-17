#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::{AtomicUsize, Ordering};

use kernel_intf::{
    info, debug,
    Lock, InterruptHandle,
    create_spinlock, acquire_spinlock, release_spinlock,
    io_complete_irp, io_set_cancel_routine, io_start_processing,
    io_install_interrupt_handler, io_remove_interrupt_handler,
    io_create_driver_worker
};
use kernel_intf::ds::RingBuffer;
use kernel_intf::driver::{
    DeviceObject, DriverObject, Irp, IrpMinor, Keystroke, RegisterHandlerInfo, Status,
    create_device,
};
use kernel_intf::mem::PoolAllocatorGlobal;

const PORT_DATA:      u16  = 0x60;
const PORT_STATUS:    u16  = 0x64;
const PORT_CMD:       u16  = 0x64;
const KS_BUF_SIZE:   usize = 64;   // Keystroke ring capacity
const MAX_PENDING:   usize = 16;
const SC_PENDING_SIZE: usize = 32;

#[cfg(target_arch = "x86_64")]
static SCANCODE_MAP: [u8; 128] = [
    // 0x00
    0,
    // 0x01 – Escape
    0x1B,
    // 0x02–0x0B – '1' through '0'
    b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'0',
    // 0x0C–0x0D – '-', '='
    b'-', b'=',
    // 0x0E – Backspace
    0x08,
    // 0x0F – Tab
    b'\t',
    // 0x10–0x19 – qwertyuiop
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p',
    // 0x1A–0x1B – '[', ']'
    b'[', b']',
    // 0x1C – Enter
    b'\n',
    // 0x1D – Left Ctrl
    0,
    // 0x1E–0x26 – asdfghjkl
    b'a', b's', b'd', b'f', b'g', b'h', b'j', b'k', b'l',
    // 0x27–0x29 – ';', '\'', '`'
    b';', b'\'', b'`',
    // 0x2A – Left Shift
    0,
    // 0x2B – backslash
    b'\\',
    // 0x2C–0x32 – zxcvbnm
    b'z', b'x', b'c', b'v', b'b', b'n', b'm',
    // 0x33–0x35 – ',', '.', '/'
    b',', b'.', b'/',
    // 0x36 – Right Shift
    0,
    // 0x37 – Keypad *
    b'*',
    // 0x38 – Left Alt
    0,
    // 0x39 – Space
    b' ',
    // 0x3A – Caps Lock
    0,
    // 0x3B–0x44 – F1–F10
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0x45 – Num Lock
    0,
    // 0x46 – Scroll Lock
    0,
    // 0x47–0x53 – Keypad 7–9, -, 4–6, +, 1–3, 0, .
    b'7', b'8', b'9', b'-', b'4', b'5', b'6', b'+', b'1', b'2', b'3', b'0', b'.',
    // 0x54–0x58 – unmapped / F11 / F12
    0, 0, 0, 0, 0,
    // 0x59–0x7F – pad to 128 (39 zeros)
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0,
];

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    unsafe {
        core::arch::asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    val
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn outb(port: u16, val: u8) {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn write_cmd(port: u16, val: u8) {
    unsafe {
        for _ in 0..0x10000_usize {
            if inb(PORT_STATUS) & 0x02 == 0 { break; }
        }
        outb(port, val);
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn flush_obuf() {
    unsafe {
        for _ in 0..16_usize {
            if inb(PORT_STATUS) & 0x01 == 0 { break; }
            let _ = inb(PORT_DATA);
        }
    }
}

#[derive(Copy, Clone)]
struct PendingEntry {
    irp:       *mut Irp,
    // Number of Keystroke structs requested (NOT bytes).
    requested: usize
}

struct I8042Ctx {
    lock:             Lock,
    ks_ring:          RingBuffer<Keystroke, KS_BUF_SIZE>,
    sc_pending:       RingBuffer<Keystroke, SC_PENDING_SIZE>,
    pending:          [PendingEntry; MAX_PENDING],
    pending_len:      usize,
    keystroke_handler: Option<RegisterHandlerInfo>,
    interrupt_handle: InterruptHandle
}

unsafe impl Send for I8042Ctx {}
unsafe impl Sync for I8042Ctx {}

impl I8042Ctx {
    const fn zeroed() -> Self {
        Self {
            lock:             Lock { lock: 0, int_status: false },
            ks_ring:          RingBuffer::new(Keystroke { scancode: 0, ascii: 0, flags: 0 }),
            sc_pending:       RingBuffer::new(Keystroke { scancode: 0, ascii: 0, flags: 0 }),
            pending:          [PendingEntry { irp: core::ptr::null_mut(), requested: 0 }; MAX_PENDING],
            pending_len:      0,
            keystroke_handler: None,
            interrupt_handle: InterruptHandle { irq: 0, node_ptr: 0 },
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

static DEVICE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[kmod::init(driver)]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("{} initializing (id={})...", driver.get_name(), driver.id);

    kmod::dispatch_init!(
        driver,
        dispatch_add,
        dispatch_pnp,
        dispatch_read,
        dispatch_control
    );

    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, pdo: Option<&DeviceObject>) -> Status {
    let idx = DEVICE_COUNT.fetch_add(1, Ordering::Relaxed);
    let name: &'static str = alloc::boxed::Box::leak(
        alloc::format!("ps/2_port{}", idx).into_boxed_str()
    );

    let ctx = alloc::boxed::Box::new_in(I8042Ctx::zeroed(), PoolAllocatorGlobal);
    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(ctx).0 as *mut c_void;

    let dev = create_device(driver, Some(name), ctx_ptr, pdo, false);
    if dev.is_null() {
        info!("i8042: create_device failed for {}", name);
        unsafe { drop(alloc::boxed::Box::from_raw_in(ctx_ptr as *mut I8042Ctx, PoolAllocatorGlobal)); }
        return Status::Failed;
    }

    info!("i8042: added device '{}'", name);
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
    info!("i8042: start '{}'", device.get_name().unwrap_or("?"));

    let ctx = unsafe { &mut *(device.ctx as *mut I8042Ctx) };

    create_spinlock(&mut ctx.lock);
    ctx.ks_ring    = RingBuffer::new(Keystroke { scancode: 0, ascii: 0, flags: 0 });
    ctx.sc_pending = RingBuffer::new(Keystroke { scancode: 0, ascii: 0, flags: 0 });
    ctx.pending_len = 0;

    #[cfg(target_arch = "x86_64")]
    unsafe {
        flush_obuf();
        write_cmd(PORT_CMD, 0xAE);
        write_cmd(PORT_DATA, 0xF4);

        for _ in 0..0x10000_usize {
            if inb(PORT_STATUS) & 0x01 != 0 {
                let ack = inb(PORT_DATA);
                debug!("i8042: scan-enable ack = {:#04x}", ack);
                break;
            }
        }
    }

    ctx.interrupt_handle = io_install_interrupt_handler(1, device.ctx, keyboard_isr, true, true);

    request.complete_irp(Status::Success);
    Status::Success
}

fn do_stop(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("i8042: stop '{}'", device.get_name().unwrap_or("?"));

    let ctx = unsafe { &mut *(device.ctx as *mut I8042Ctx) };

    io_remove_interrupt_handler(ctx.interrupt_handle);

    let mut to_fail = [core::ptr::null_mut::<Irp>(); MAX_PENDING];
    let to_fail_len;

    acquire_spinlock(&mut ctx.lock);
    to_fail_len = ctx.pending_len;
    for i in 0..to_fail_len {
        to_fail[i] = ctx.pending[i].irp;
    }
    ctx.pending_len = 0;
    release_spinlock(&mut ctx.lock);

    for i in 0..to_fail_len {
        info!("Cancelling {} irp(s) as part of device stop", to_fail_len);
        io_complete_irp(to_fail[i], Status::Failed);
    }

    request.complete_irp(Status::Success);
    Status::Success
}

fn do_remove(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("i8042: remove '{}'", device.get_name().unwrap_or("?"));

    if !device.ctx.is_null() {
        unsafe {
            drop(alloc::boxed::Box::from_raw_in(
                device.ctx as *mut I8042Ctx,
                PoolAllocatorGlobal
            ));
        }
    }

    request.complete_irp(Status::Success);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_control(device: &DeviceObject, request: &mut Irp) -> Status {
    match request.minor_code {
        IrpMinor::RegisterKeyboardHandler => {
            info!("Received register keyboard handler request from class driver");
            let handler_info = unsafe { request.req_info.register_handler };
            let ctx = unsafe { &mut *(device.ctx as *mut I8042Ctx) };
            acquire_spinlock(&mut ctx.lock);
            ctx.keystroke_handler = Some(handler_info);
            release_spinlock(&mut ctx.lock);
            request.complete_irp(Status::Success);
            Status::Success
        }
        _ => Status::Unsupported
    }
}

#[kmod::dispatch_handler]
fn dispatch_read(device: &DeviceObject, request: &mut Irp) -> Status {
    let ks_size  = size_of::<Keystroke>();
    let buf_size = request.buffer.size;

    if buf_size == 0 || buf_size % ks_size != 0 {
        if buf_size == 0 {
            info!("Buffer size of 0 not allowed");
        }
        else {
            info!("Buffer size not multiple of Keystroke struct size!");
        }
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    let requested = buf_size / ks_size;
    if requested > KS_BUF_SIZE {
        info!("Number of keystroke packets requested > MAX({})", KS_BUF_SIZE);
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    let ctx = unsafe { &mut *(device.ctx as *mut I8042Ctx) };

    acquire_spinlock(&mut ctx.lock);

    if ctx.ks_ring.len() >= requested {
        let dst = request.buffer.base_address as *mut Keystroke;
        unsafe { ctx.ks_ring.dequeue_into(dst, requested); }
        request.bytes_completed = requested * ks_size;
        release_spinlock(&mut ctx.lock);
        request.complete_irp(Status::Success);
        return Status::Success;
    }

    if ctx.pending_len == MAX_PENDING {
        release_spinlock(&mut ctx.lock);
        info!("Too many irq's pending. Cancelling this one..");
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    let irp_ptr = request as *mut Irp;
    ctx.pending[ctx.pending_len] = PendingEntry { irp: irp_ptr, requested };
    ctx.pending_len += 1;
    release_spinlock(&mut ctx.lock);

    io_set_cancel_routine(request, i8042_cancel);

    Status::Pending
}

extern "C" fn i8042_cancel(dev: *const DeviceObject, irp: *mut Irp) {
    if !dev.is_null() {
        let ctx = unsafe { &mut *((*dev).ctx as *mut I8042Ctx) };
        acquire_spinlock(&mut ctx.lock);
        ctx.remove_pending(irp);
        release_spinlock(&mut ctx.lock);
    }
    io_complete_irp(irp, Status::Cancelled);
}

// ISR — decode one scancode into a Keystroke and stash it in sc_pending.
// Key-release events (bit 7) are recorded with flags=1 but ASCII=0.
extern "C" fn keyboard_isr(ctx_ptr: *mut c_void) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        let raw_sc = unsafe { inb(PORT_DATA) };
        let is_release = raw_sc & 0x80 != 0;
        let scancode = raw_sc & 0x7F;

        let idx = scancode as usize;
        let ascii = if idx < SCANCODE_MAP.len() { SCANCODE_MAP[idx] } else { 0 };

        let ks = Keystroke { scancode, ascii, flags: if is_release {1} else {0} };

        let ctx = unsafe { &mut *(ctx_ptr as *mut I8042Ctx) };

        acquire_spinlock(&mut ctx.lock);
        ctx.sc_pending.push(ks);
        release_spinlock(&mut ctx.lock);

        let _ = io_create_driver_worker(keyboard_dw, ctx_ptr);
    }

    true
}

// Driver worker — batch-drain sc_pending, push to ring, satisfy pending reads,
// then call the registered keystroke handler — all with a single lock window.
extern "C" fn keyboard_dw(ctx_ptr: *mut c_void) {
    let ctx = unsafe { &mut *(ctx_ptr as *mut I8042Ctx) };
    let ks_size = size_of::<Keystroke>();

    let mut batch = [Keystroke::default(); SC_PENDING_SIZE];

    acquire_spinlock(&mut ctx.lock);

    let batch_len = ctx.sc_pending.len();
    unsafe { ctx.sc_pending.dequeue_into(batch.as_mut_ptr(), batch_len); }

    for i in 0..batch_len {
        ctx.ks_ring.push(batch[i]);
    }

    let avail = ctx.ks_ring.len();
    let mut max_consumed = 0usize;
    let mut collected     = [core::ptr::null_mut::<Irp>(); MAX_PENDING];
    let mut collected_len = 0;
    let mut i = 0;

    while i < ctx.pending_len {
        let entry = ctx.pending[i];
        let already_given = unsafe { (*entry.irp).bytes_completed } / ks_size;
        let remaining = entry.requested - already_given;
        let give = avail.min(remaining);

        if give > 0 {
            let dst = unsafe {
                ((*entry.irp).buffer.base_address as *mut Keystroke).add(already_given)
            };
            unsafe { ctx.ks_ring.peek_into(dst, give); }
            unsafe { (*entry.irp).bytes_completed += give * ks_size; }
            if give > max_consumed { max_consumed = give; }
        }

        if unsafe { (*entry.irp).bytes_completed } == entry.requested * ks_size {
            ctx.pending[i] = ctx.pending[ctx.pending_len - 1];
            ctx.pending_len -= 1;
            collected[collected_len] = entry.irp;
            collected_len += 1;
        } else {
            i += 1;
        }
    }

    ctx.ks_ring.advance(max_consumed);

    let maybe_handler = ctx.keystroke_handler;

    release_spinlock(&mut ctx.lock);

    // Invoke the registered input-layer handler outside the lock.
    if let Some(info) = maybe_handler {
        unsafe { (info.handler)(batch.as_ptr(), batch_len, info.context); }
    }

    // Complete satisfied read IRPs outside the lock.
    for k in 0..collected_len {
        let irp = collected[k];
        if io_start_processing(irp) {
            io_complete_irp(irp, Status::Success);
        }
    }
}
