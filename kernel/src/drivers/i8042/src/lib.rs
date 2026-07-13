#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::{AtomicUsize, Ordering};

use kernel_intf::{
    KInterruptHandle, Lock, RemoveLock, acquire_spinlock, create_spinlock, debug, info, io_complete_irp, io_create_driver_worker, io_install_interrupt_handler, io_remove_interrupt_handler, io_set_cancel_routine, io_start_processing, release_spinlock
};
use kernel_intf::ds::RingBuffer;
use kernel_intf::driver::{
    DeviceObject, DeviceType, DriverObject, Irp, IrpMinor, Keystroke, RegisterHandlerInfo, ResEntry, ResType, Status,
    create_device,
};
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::hw::{inb, outb};

// Default PS/2 I/O ports — overridden by resource list in do_start if provided
const DEFAULT_PORT_DATA: u16 = 0x60;
const DEFAULT_PORT_CMD:  u16 = 0x64;
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
unsafe fn write_cmd(port_cmd: u16, port: u16, val: u8) {
    unsafe {
        for _ in 0..0x10000_usize {
            if inb(port_cmd) & 0x02 == 0 { break; }
        }
        outb(port, val);
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn flush_obuf(port_data: u16, port_cmd: u16) {
    unsafe {
        for _ in 0..16_usize {
            if inb(port_cmd) & 0x01 == 0 { break; }
            let _ = inb(port_data);
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
    keystroke_handler: RegisterHandlerInfo,
    interrupt_handle: KInterruptHandle,
    port_data:        u16,
    port_cmd:         u16,
    remove_lock:      RemoveLock,
    // Decode state carried across interrupts.
    extended_prefix:  bool,
    pause_skip:       u8,
    ctrl_held:        bool,
    shift_held:       bool,
    alt_held:         bool,
    caps_lock:        bool,
    num_lock:         bool
}

unsafe impl Send for I8042Ctx {}
unsafe impl Sync for I8042Ctx {}

impl I8042Ctx {
    const fn zeroed() -> Self {
        Self {
            lock:             Lock::new(),
            ks_ring:          RingBuffer::new(Keystroke { scancode: 0, ascii: 0, flags: 0, modifiers: 0 }),
            sc_pending:       RingBuffer::new(Keystroke { scancode: 0, ascii: 0, flags: 0, modifiers: 0 }),
            pending:          [PendingEntry { irp: core::ptr::null_mut(), requested: 0 }; MAX_PENDING],
            pending_len:      0,
            keystroke_handler: RegisterHandlerInfo::new(),
            interrupt_handle: KInterruptHandle::new(),
            port_data:        DEFAULT_PORT_DATA,
            port_cmd:         DEFAULT_PORT_CMD,
            remove_lock:      RemoveLock::new(),
            extended_prefix:  false,
            pause_skip:       0,
            ctrl_held:        false,
            shift_held:       false,
            alt_held:         false,
            caps_lock:        false,
            num_lock:         false
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
fn dispatch_add(driver: &DriverObject, pdo: &DeviceObject) -> Status {
    let idx = DEVICE_COUNT.fetch_add(1, Ordering::Relaxed);
    let name: &'static str = alloc::boxed::Box::leak(
        alloc::format!("ps/2_port{}", idx).into_boxed_str()
    );

    let ctx = alloc::boxed::Box::new_in(I8042Ctx::zeroed(), PoolAllocatorGlobal);
    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(ctx).0 as *mut c_void;

    let dev = create_device(driver, Some(name), ctx_ptr, Some(pdo), false, DeviceType::None);
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
    ctx.ks_ring    = RingBuffer::new(Keystroke { scancode: 0, ascii: 0, flags: 0, modifiers: 0 });
    ctx.sc_pending = RingBuffer::new(Keystroke { scancode: 0, ascii: 0, flags: 0, modifiers: 0 });
    ctx.pending_len = 0;
    ctx.extended_prefix = false;
    ctx.pause_skip = 0;
    ctx.ctrl_held = false;
    ctx.shift_held = false;
    ctx.alt_held = false;
    ctx.caps_lock = false;
    ctx.num_lock = false;

    // Read I/O port addresses and interrupt vector from the resource list.
    let res_list = unsafe { request.req_info.res_list };
    let res_slice: &[ResEntry] = unsafe { core::slice::from_raw_parts(res_list.base, res_list.count) };
    let mut irq = 0usize;
    let mut vector = 0usize;
    let mut active_high = true;
    let mut edge_triggered = true;
    for entry in res_slice {
        match entry.res_type {
            ResType::Interrupt => {
                irq           = unsafe { entry.desc.interrupt.irq };
                vector        = unsafe { entry.desc.interrupt.vector };
                active_high   = unsafe { entry.desc.interrupt.active_high };
                edge_triggered = unsafe { entry.desc.interrupt.edge_triggered };
            }
            ResType::Port => {
                let base = unsafe { entry.desc.port.base as u16 };
                // 0x60 = data register, 0x64 = command/status register
                if base == DEFAULT_PORT_DATA {
                    ctx.port_data = base;
                } else {
                    ctx.port_cmd = base;
                }
            }
            _ => {}
        }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe {
        flush_obuf(ctx.port_data, ctx.port_cmd);
        write_cmd(ctx.port_cmd, ctx.port_cmd, 0xAE);
        write_cmd(ctx.port_cmd, ctx.port_data, 0xF4);

        for _ in 0..0x10000_usize {
            if inb(ctx.port_cmd) & 0x01 != 0 {
                let ack = inb(ctx.port_data);
                debug!("i8042: scan-enable ack = {:#04x}", ack);
                break;
            }
        }
    }

    ctx.interrupt_handle = io_install_interrupt_handler(vector, irq as isize, device.ctx, keyboard_isr, active_high, edge_triggered);

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
    if let Some(name) = device.get_name() {
        unsafe { drop(alloc::boxed::Box::from_raw(name as *const str as *mut str)); }
    }

    if !device.ctx.is_null() {
        let ctx = unsafe { &mut *(device.ctx as *mut I8042Ctx) };
        // If a keyboard_dw call is still queued against this ctx, it holds
        // the last reference and will free it when it releases.
        if ctx.remove_lock.begin_remove() {
            unsafe {
                drop(alloc::boxed::Box::from_raw_in(
                    device.ctx as *mut I8042Ctx,
                    PoolAllocatorGlobal
                ));
            }
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
            ctx.keystroke_handler = handler_info;
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

    let avail = ctx.ks_ring.len();
    if avail > 0 {
        let give = avail.min(requested);
        let dst = request.buffer.base_address as *mut Keystroke;
        unsafe { ctx.ks_ring.dequeue_into(dst, give); }
        request.bytes_completed = give * ks_size;
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

    if !io_set_cancel_routine(request, i8042_cancel) {
        // Already cancelled before we could register -- nobody else will
        // complete this irp, so undo our own queue insertion and do it.
        acquire_spinlock(&mut ctx.lock);
        ctx.remove_pending(irp_ptr);
        release_spinlock(&mut ctx.lock);
        request.complete_irp(Status::Cancelled);
        return Status::Cancelled;
    }

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

// Base (non-extended) scancodes for the modifier keys.
const SC_CTRL:       u8 = 0x1D;
const SC_LSHIFT:     u8 = 0x2A;
const SC_RSHIFT:     u8 = 0x36;
const SC_ALT:        u8 = 0x38;
const SC_CAPSLOCK:   u8 = 0x3A;
const SC_NUMLOCK:    u8 = 0x45;

fn current_modifiers(ctx: &I8042Ctx) -> u8 {
    let mut m = 0u8;
    if ctx.ctrl_held  { m |= kernel_intf::driver::MOD_CTRL; }
    if ctx.shift_held { m |= kernel_intf::driver::MOD_SHIFT; }
    if ctx.alt_held   { m |= kernel_intf::driver::MOD_ALT; }
    if ctx.caps_lock  { m |= kernel_intf::driver::MOD_CAPSLOCK; }
    if ctx.num_lock   { m |= kernel_intf::driver::MOD_NUMLOCK; }
    m
}

// ISR — decode one scancode into a Keystroke and stash it in sc_pending.
// Handles the 0xE0 extended-key prefix and 0xE1 Pause/Break sequence, and
// tracks modifier key (Ctrl/Shift/Alt/CapsLock/NumLock) state across calls.
// Key-release events (bit 7) are recorded with flags bit 0 set but ASCII=0.
extern "C" fn keyboard_isr(ctx_ptr: *mut c_void) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        let ctx = unsafe { &mut *(ctx_ptr as *mut I8042Ctx) };

        if !ctx.remove_lock.acquire() {
            return true;
        }

        let raw_sc = unsafe { inb(ctx.port_data) };

        if raw_sc == 0xE0 {
            ctx.extended_prefix = true;
            if ctx.remove_lock.release() {
                unsafe { drop(alloc::boxed::Box::from_raw_in(ctx_ptr as *mut I8042Ctx, PoolAllocatorGlobal)); }
            }
            return true;
        }

        if raw_sc == 0xE1 {
            // Pause/Break sends a fixed 6-byte sequence with no break code;
            // discard the remaining 5 bytes without touching any state.
            ctx.pause_skip = 5;
            if ctx.remove_lock.release() {
                unsafe { drop(alloc::boxed::Box::from_raw_in(ctx_ptr as *mut I8042Ctx, PoolAllocatorGlobal)); }
            }
            return true;
        }

        if ctx.pause_skip > 0 {
            ctx.pause_skip -= 1;
            if ctx.remove_lock.release() {
                unsafe { drop(alloc::boxed::Box::from_raw_in(ctx_ptr as *mut I8042Ctx, PoolAllocatorGlobal)); }
            }
            return true;
        }

        let extended = ctx.extended_prefix;
        ctx.extended_prefix = false;

        let is_release = raw_sc & 0x80 != 0;
        let scancode = raw_sc & 0x7F;

        match scancode {
            SC_CTRL => ctx.ctrl_held = !is_release,
            SC_LSHIFT | SC_RSHIFT if !extended => ctx.shift_held = !is_release,
            SC_ALT => ctx.alt_held = !is_release,
            SC_CAPSLOCK if !is_release => ctx.caps_lock = !ctx.caps_lock,
            SC_NUMLOCK if !is_release && !extended => ctx.num_lock = !ctx.num_lock,
            _ => {}
        }

        let idx = scancode as usize;
        let ascii = if idx < SCANCODE_MAP.len() { SCANCODE_MAP[idx] } else { 0 };

        let flags = (is_release as u8) | ((extended as u8) << 1);
        let modifiers = current_modifiers(ctx);

        let ks = Keystroke { scancode, ascii, flags, modifiers };

        acquire_spinlock(&mut ctx.lock);
        ctx.sc_pending.push(ks);
        release_spinlock(&mut ctx.lock);

        if io_create_driver_worker(keyboard_dw, ctx_ptr).is_err() {
            // Nothing will ever call keyboard_dw's release() for this
            // reference now -- release it ourselves.
            if ctx.remove_lock.release() {
                unsafe { drop(alloc::boxed::Box::from_raw_in(ctx_ptr as *mut I8042Ctx, PoolAllocatorGlobal)); }
            }
        }
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

    let mut satisfied = 0usize;
    let mut collected = [core::ptr::null_mut::<Irp>(); MAX_PENDING];
    let mut collected_len = 0;

    while ctx.ks_ring.len() > 0 && satisfied < ctx.pending_len {
        let entry = ctx.pending[satisfied];
        let give = ctx.ks_ring.len().min(entry.requested);
        let dst = unsafe { (*entry.irp).buffer.base_address as *mut Keystroke };
        unsafe { ctx.ks_ring.dequeue_into(dst, give); }
        unsafe { (*entry.irp).bytes_completed = give * ks_size; }
        collected[collected_len] = entry.irp;
        collected_len += 1;
        satisfied += 1;
    }

    let remaining = ctx.pending_len - satisfied;
    for i in 0..remaining {
        ctx.pending[i] = ctx.pending[satisfied + i];
    }
    ctx.pending_len = remaining;

    let class_handler = ctx.keystroke_handler;

    release_spinlock(&mut ctx.lock);

    // Invoke the registered input-layer handler outside the lock.
    if let Some(handler) = class_handler.handler {
        unsafe { (handler)(batch.as_ptr(), batch_len, class_handler.context); }
    }

    // Complete satisfied read IRPs outside the lock.
    for k in 0..collected_len {
        let irp = collected[k];
        if io_start_processing(irp) {
            io_complete_irp(irp, Status::Success);
        }
    }

    // Release the reference taken in keyboard_isr; if do_remove already ran
    // and this was the last outstanding reference, we free ctx.
    if ctx.remove_lock.release() {
        unsafe { drop(alloc::boxed::Box::from_raw_in(ctx_ptr as *mut I8042Ctx, PoolAllocatorGlobal)); }
    }
}

#[kmod::driver_unload]
fn destroy(driver: &mut DriverObject) {
    info!("Destroying driver {}", driver.get_name());
}