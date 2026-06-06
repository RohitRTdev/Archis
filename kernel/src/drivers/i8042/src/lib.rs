#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use kernel_intf::{
    info, debug,
    Lock,
    create_spinlock, acquire_spinlock, release_spinlock,
    io_complete_irp, io_set_cancel_routine, io_start_processing,
    install_interrupt_handler_ffi,
};
use kernel_intf::driver::{
    DeviceObject, DriverObject, Irp, IrpMinor, Status, create_device,
};
use kernel_intf::mem::PoolAllocatorGlobal;

const PORT_DATA:   u16 = 0x60;
const PORT_STATUS: u16 = 0x64;
const PORT_CMD:    u16 = 0x64;
const CHAR_BUF_SIZE: usize = 256;
const MAX_PENDING:   usize = 16;

// Scan-code set 1 make-code → ASCII. Index 0x00 unused; 0x01–0x58 mapped;
// 0x59–0x7F padded with 0 (39 zeros). Total: 128 entries.
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

// Spin until the input buffer is clear, then write `val` to `port`.
#[cfg(target_arch = "x86_64")]
unsafe fn write_cmd(port: u16, val: u8) {
    unsafe {
        for _ in 0..0x10000_usize {
            if inb(PORT_STATUS) & 0x02 == 0 { break; }
        }
        outb(port, val);
    }
}

// Drain the controller's output buffer (OBF bit = 0x01 in status).
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
    requested: usize
}

struct I8042Ctx {
    lock:        Lock,
    char_ring:   [u8; CHAR_BUF_SIZE],
    char_head:   usize,
    char_tail:   usize,
    char_len:    usize,
    pending:     [PendingEntry; MAX_PENDING],
    pending_len: usize
}

unsafe impl Send for I8042Ctx {}
unsafe impl Sync for I8042Ctx {}

impl I8042Ctx {
    const fn zeroed() -> Self {
        Self {
            lock:        Lock { lock: 0, int_status: false },
            char_ring:   [0; CHAR_BUF_SIZE],
            char_head:   0,
            char_tail:   0,
            char_len:    0,
            pending:     [PendingEntry { irp: core::ptr::null_mut(), requested: 0 }; MAX_PENDING],
            pending_len: 0,
        }
    }

    fn push_char(&mut self, ch: u8) {
        if self.char_len < CHAR_BUF_SIZE {
            self.char_ring[self.char_tail] = ch;
            self.char_tail = (self.char_tail + 1) % CHAR_BUF_SIZE;
            self.char_len += 1;
        }
        // If the ring is full we silently drop – the reader hasn't caught up yet.
    }

    // Dequeue `n` bytes into `dst`. Caller must ensure `char_len >= n`.
    unsafe fn dequeue_into(&mut self, dst: *mut u8, n: usize) {
        for i in 0..n {
            unsafe { dst.add(i).write(self.char_ring[self.char_head]); }
            self.char_head = (self.char_head + 1) % CHAR_BUF_SIZE;
            self.char_len -= 1;
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

static DEVICE_PTR: AtomicPtr<DeviceObject> = AtomicPtr::new(core::ptr::null_mut());
static DEVICE_COUNT: AtomicUsize = AtomicUsize::new(0);

// We support up to two PS/2 ports
static DEVICE_NAMES: [&str; 2] = ["ps/2_port0", "ps/2_port1"];

#[kmod::init]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("{} initializing (id={})...", driver.get_name(), driver.id);

    kmod::dispatch_init!(
        driver,
        dispatch_add,
        dispatch_pnp,
        dispatch_read
    );

    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, pdo: Option<&DeviceObject>) -> Status {
    let idx = DEVICE_COUNT.fetch_add(1, Ordering::Relaxed);
    if idx >= DEVICE_NAMES.len() {
        info!("i8042: too many devices (idx={}), rejecting", idx);
        return Status::Failed;
    }
    let name = DEVICE_NAMES[idx];

    let ctx = alloc::boxed::Box::new_in(I8042Ctx::zeroed(), PoolAllocatorGlobal);
    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(ctx).0 as *mut c_void;

    let dev = create_device(driver, Some(name), ctx_ptr, pdo);
    if dev.is_null() {
        info!("i8042: create_device failed for {}", name);
        // Free ctx since kernel won't call remove
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

    // Initialise the spinlock and reset all state before enabling the IRQ.
    unsafe { create_spinlock(&mut ctx.lock); }
    ctx.char_head   = 0;
    ctx.char_tail   = 0;
    ctx.char_len    = 0;
    ctx.pending_len = 0;

    // Hardware initialisation (x86_64 only — hardcoded resources, no ACPI yet).
    #[cfg(target_arch = "x86_64")]
    unsafe {
        flush_obuf();                  // drain stale bytes
        write_cmd(PORT_CMD, 0xAE);     // enable first PS/2 port
        write_cmd(PORT_DATA, 0xF4);    // enable keyboard scanning

        // Wait for the ACK byte (0xFA); give up after a short spin.
        for _ in 0..0x10000_usize {
            if inb(PORT_STATUS) & 0x01 != 0 {
                let ack = inb(PORT_DATA);
                debug!("i8042: scan-enable ack = {:#04x}", ack);
                break;
            }
        }
    }

    // Publish device pointer so the ISR can find the ctx.
    DEVICE_PTR.store(device as *const _ as *mut DeviceObject, Ordering::Release);

    // Install keyboard interrupt (IRQ 1, active-high, edge-triggered).
    unsafe { install_interrupt_handler_ffi(1, keyboard_isr, true, true); }

    request.complete_irp(Status::Success);
    Status::Success
}

fn do_stop(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("i8042: stop '{}'", device.get_name().unwrap_or("?"));

    let ctx = unsafe { &mut *(device.ctx as *mut I8042Ctx) };

    // Collect all pending IRPs under the lock, clear the list.
    let mut to_fail = [core::ptr::null_mut::<Irp>(); MAX_PENDING];
    let to_fail_len;

    unsafe { acquire_spinlock(&mut ctx.lock); }
    to_fail_len = ctx.pending_len;
    for i in 0..to_fail_len {
        to_fail[i] = ctx.pending[i].irp;
    }
    ctx.pending_len = 0;
    // Prevent the ISR from touching a stopped device.
    DEVICE_PTR.store(core::ptr::null_mut(), Ordering::Release);
    unsafe { release_spinlock(&mut ctx.lock); }

    // Fail all queued IRPs outside the lock (device stopping, not user cancellation).
    for i in 0..to_fail_len {
        unsafe { io_complete_irp(to_fail[i], Status::Failed); }
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
                PoolAllocatorGlobal,
            ));
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
    if requested > CHAR_BUF_SIZE {
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    let ctx = unsafe { &mut *(device.ctx as *mut I8042Ctx) };

    unsafe { acquire_spinlock(&mut ctx.lock); }

    if ctx.char_len >= requested {
        // Enough characters already buffered – satisfy synchronously.
        let dst = request.buffer.base_address as *mut u8;
        unsafe { ctx.dequeue_into(dst, requested); }
        request.bytes_completed = requested;
        unsafe { release_spinlock(&mut ctx.lock); }
        request.complete_irp(Status::Success);
        return Status::Success;
    }

    if ctx.pending_len == MAX_PENDING {
        unsafe { release_spinlock(&mut ctx.lock); }
        request.complete_irp(Status::Failed);
        return Status::Failed;
    }

    // Queue the IRP; the ISR will satisfy it when enough chars arrive.
    let irp_ptr = request as *mut Irp;
    ctx.pending[ctx.pending_len] = PendingEntry { irp: irp_ptr, requested };
    ctx.pending_len += 1;
    unsafe { release_spinlock(&mut ctx.lock); }

    // Install cancel routine AFTER the IRP is in our list so the cancel
    // path always finds it and can remove it cleanly.
    unsafe { io_set_cancel_routine(irp_ptr, i8042_cancel); }

    Status::Pending
}

// Cancel routine 

extern "C" fn i8042_cancel(dev: *const DeviceObject, irp: *mut Irp) {
    // Find and remove the IRP from the pending list.
    if !dev.is_null() {
        let ctx = unsafe { &mut *((*dev).ctx as *mut I8042Ctx) };
        unsafe { acquire_spinlock(&mut ctx.lock); }
        ctx.remove_pending(irp);
        unsafe { release_spinlock(&mut ctx.lock); }
    }
    // Always complete with Cancelled. If the ISR already removed the IRP from
    // the list and wrote data to its buffer, io_start_processing (called by the
    // ISR after releasing our lock) will see is_cancelled=true and bail out,
    // letting this path win.
    unsafe { io_complete_irp(irp, Status::Cancelled); }
}

// Keyboard ISR (extern "C", bare fn pointer) 

extern "C" fn keyboard_isr(_vector: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let scancode = unsafe { inb(PORT_DATA) };

        // Bit 7 set = key-release; ignore it.
        if scancode & 0x80 != 0 {
            return;
        }

        let idx = scancode as usize;
        if idx >= SCANCODE_MAP.len() {
            return;
        }
        let ch = SCANCODE_MAP[idx];
        if ch == 0 {
            return; // modifier or unmapped key
        }

        // Reach the per-device ctx through the global device pointer.
        let dev = DEVICE_PTR.load(Ordering::Acquire);
        if dev.is_null() {
            return;
        }
        let ctx = unsafe { &mut *((*dev).ctx as *mut I8042Ctx) };

        // collect satisfiable IRPs under the lock 
        let mut collected = [core::ptr::null_mut::<Irp>(); MAX_PENDING];
        let mut collected_len = 0;

        unsafe { acquire_spinlock(&mut ctx.lock); }

        ctx.push_char(ch);

        let mut i = 0;
        while i < ctx.pending_len {
            let entry = ctx.pending[i];
            if ctx.char_len >= entry.requested {
                let dst = unsafe { (*entry.irp).buffer.base_address as *mut u8 };
                unsafe { ctx.dequeue_into(dst, entry.requested); }
                unsafe { (*entry.irp).bytes_completed = entry.requested; }
                // Swap-remove from pending list.
                ctx.pending[i] = ctx.pending[ctx.pending_len - 1];
                ctx.pending_len -= 1;
                collected[collected_len] = entry.irp;
                collected_len += 1;
            } else {
                i += 1;
            }
        }

        unsafe { release_spinlock(&mut ctx.lock); }

        // complete collected IRPs outside the lock
        // io_start_processing atomically claims each IRP (prevents cancel from
        // firing after this point). If cancel already won, it returns false and
        // the IRP has been completed by the cancel path; we just move on.
        for k in 0..collected_len {
            let irp = collected[k];
            if unsafe { io_start_processing(irp) } {
                unsafe { io_complete_irp(irp, Status::Success); }
            }
        }
    }
}
