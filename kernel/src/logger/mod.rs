mod framebuffer_logger;
pub mod kring;

use framebuffer_logger::FRAMEBUFFER_LOGGER;
use crate::{devices::uart, logger::framebuffer_logger::flush_log};
use crate::hal;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
pub use framebuffer_logger::relocate_framebuffer;

static PANIC_MODE: AtomicBool = AtomicBool::new(false);
static PANIC_CORE: AtomicU8 = AtomicU8::new(0);
static IS_TTY_MODE: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
pub extern "C" fn clear_screen() {
    FRAMEBUFFER_LOGGER.lock().clear_screen();
}

fn disable_cursor() {
    FRAMEBUFFER_LOGGER.lock().disable_cursor();
}

pub fn set_panic_mode(core: u8) {
    PANIC_MODE.store(true, Ordering::Release);
    PANIC_CORE.store(core, Ordering::Release);
    disable_cursor();
    clear_screen();
}

// It is important to ensure that caller holds the screen lock before calling this function
#[unsafe(no_mangle)]
extern "C" fn serial_print_ffi(s: *const u8, len: usize) {
    let s = unsafe {
        let slice = core::slice::from_raw_parts(s, len);
        core::str::from_utf8_unchecked(slice)
    };

    let panicking = PANIC_MODE.load(Ordering::Acquire);
    let is_panic_core = PANIC_CORE.load(Ordering::Acquire) == hal::get_core() as u8;
    let tty = IS_TTY_MODE.load(Ordering::Acquire);

    // Silent cores during a panic produce no output.
    if panicking && !is_panic_core {
        return;
    }

    // Always push to the kernel ring buffer except during a panic.
    if !panicking {
        kring::push(s);
    }

    // In TTY mode outside a panic, ring buffer only — skip serial and framebuffer.
    if tty && !panicking {
        return;
    }

    uart::SERIAL.lock().write(s);
    FRAMEBUFFER_LOGGER.lock().write(s);
    flush_log();
}

// Write to framebuffer only when TTY mode is active and there is no ongoing panic.
#[unsafe(no_mangle)]
extern "C" fn tty_print_ffi(s: *const u8, len: usize) {
    if PANIC_MODE.load(Ordering::Acquire) || !IS_TTY_MODE.load(Ordering::Acquire) {
        return;
    }
    let s = unsafe {
        let slice = core::slice::from_raw_parts(s, len);
        core::str::from_utf8_unchecked(slice)
    };
    FRAMEBUFFER_LOGGER.lock().write(s);
    flush_log();
}

#[unsafe(no_mangle)]
pub extern "C" fn enable_tty_mode_ffi() {
    if PANIC_MODE.load(Ordering::Acquire) { return; }
    IS_TTY_MODE.store(true, Ordering::Release);
    FRAMEBUFFER_LOGGER.lock().clear_screen();
}

#[unsafe(no_mangle)]
pub extern "C" fn disable_tty_mode_ffi() {
    if PANIC_MODE.load(Ordering::Acquire) { return; }
    IS_TTY_MODE.store(false, Ordering::Release);
    FRAMEBUFFER_LOGGER.lock().clear_screen();
}

pub fn init() {
    kernel_intf::init_logger(env!("CARGO_PKG_NAME"));
    uart::init();
    framebuffer_logger::init();
    
    // We assume RTC always exists for PC-AT systems
    kernel_intf::enable_timestamp();
}
