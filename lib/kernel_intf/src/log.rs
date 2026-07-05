#![allow(static_mut_refs)]

const LOG_SCRATCH_BUFFER_SIZE: usize = 1024;

#[macro_export]
macro_rules! print {
    () => {};
    ($($arg:tt)*) => {
        #[cfg(not(test))]
        {
            let args = ::core::format_args!($($arg)*);
            unsafe {
                use core::fmt::Write;

                $crate::acquire_spinlock(&mut $crate::LOGGER.lock);
                $crate::LOGGER.write_fmt(args).unwrap();
                $crate::LOGGER.flush();
                $crate::release_spinlock(&mut $crate::LOGGER.lock);
            }
        }
    };
}

#[macro_export]
macro_rules! println {
    () => {
        #[cfg(test)]
        {
            ::std::println!();
        }
        #[cfg(not(test))]
        {
            use core::fmt::Write;
            unsafe {
                $crate::acquire_spinlock(&mut $crate::LOGGER.lock);
                $crate::LOGGER.write_fmt(::core::format_args!("\n")).unwrap();
                $crate::LOGGER.flush();
                $crate::release_spinlock(&mut $crate::LOGGER.lock);
            }
        }
    };
    ($($arg:tt)*) => {
        #[cfg(test)]
        {
            ::std::println!($($arg)*);
        }
        #[cfg(not(test))]
        {
            let args = ::core::format_args!($($arg)*);
            unsafe {
                use core::fmt::Write;

                $crate::acquire_spinlock(&mut $crate::LOGGER.lock);
                $crate::LOGGER.write_fmt(args)
                .and_then(|_| $crate::LOGGER.write_str("\n"))
                .unwrap();
                $crate::LOGGER.flush();
                $crate::release_spinlock(&mut $crate::LOGGER.lock);
            }
        }
    };
}

#[macro_export]
macro_rules! level_print {
    ($level: literal, $($arg:tt)*) => {{
        let timestamp = unsafe {
            $crate::LOGGER.log_timestamp.load(::core::sync::atomic::Ordering::Acquire)
        };

        let name = unsafe {$crate::LOGGER.module_name};
        if timestamp {
            let rtc = unsafe {$crate::read_rtc()};
            let ts = unsafe {$crate::read_timestamp()};
            let core = unsafe {$crate::get_core_ffi()};
            $crate::println!("[{}]-[{}]-[{}]-[{}]-[{}]: {}", name, $level, rtc, ts, core, format_args!($($arg)*));
        } else {
            $crate::println!("[{}]-[{}]: {}", name, $level, format_args!($($arg)*));
        }
    }};
}


#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{
        $crate::level_print!("INFO", $($arg)*);
    }};
}

#[cfg(debug_assertions)]
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {{
        $crate::level_print!("DEBUG", $($arg)*);
    }};
}

#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {};
}

#[cfg(debug_assertions)]
#[macro_export]
macro_rules! fs_log {
    ($($arg:tt)*) => {{
        $crate::level_print!("FS", $($arg)*);
    }};
}

#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! fs_log {
    ($($arg:tt)*) => {};
}

impl core::fmt::Write for KernelLogger {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        unsafe {
            let s = s.as_bytes();
            let mut len = s.len();
            if s.len() + self.buf_size > LOG_SCRATCH_BUFFER_SIZE {
                len = LOG_SCRATCH_BUFFER_SIZE - self.buf_size;
            }

            ::core::ptr::copy_nonoverlapping(s.as_ptr(), self.scratch_buffer.as_mut_ptr().add(self.buf_size), len);
            self.buf_size += len;
        }
        Ok(())
    }
} 

impl KernelLogger {
    pub fn flush(&mut self) {
        unsafe {
            crate::serial_print_ffi(self.scratch_buffer.as_ptr(), self.buf_size);
            self.buf_size = 0;
        }
    }
}

pub struct KernelLogger {
    pub module_name: &'static str,
    pub log_timestamp: core::sync::atomic::AtomicBool,
    pub lock: crate::Lock,
    pub scratch_buffer: [u8; LOG_SCRATCH_BUFFER_SIZE],
    pub buf_size: usize
}

pub static mut LOGGER: KernelLogger = KernelLogger {
    module_name: env!("CARGO_PKG_NAME"),
    log_timestamp: core::sync::atomic::AtomicBool::new(false),
    lock: crate::Lock::new(),
    scratch_buffer: [0; LOG_SCRATCH_BUFFER_SIZE],
    buf_size: 0
};

pub fn init_logger(module_name: &'static str) {
    unsafe {
        LOGGER.module_name = module_name;
        crate::create_spinlock(&mut LOGGER.lock);
    }
}

pub fn enable_timestamp() {
    unsafe {
        crate::LOGGER.log_timestamp.store(true, core::sync::atomic::Ordering::Release);
    }
}

// Holding lock indefinitely effectively disables the logger
// It also waits for any existing cores to complete logging
pub fn disable_logger() {
    unsafe {
        crate::acquire_spinlock(&mut crate::LOGGER.lock);    
    }
}

pub fn enable_logger() {
    unsafe {
        crate::release_spinlock(&mut crate::LOGGER.lock);    
    }
}


// This is only to be called by relocation code during very early init
// This is not for randomly changing logger name of a module (not lock protected)
// That is to be decided at init time of a module
pub fn set_logger_name(module_name: &'static str) {
    unsafe {
        crate::LOGGER.module_name = module_name;
    }
}
