#[cfg(all(debug_assertions, feature = "debug-scheduler-logs"))]
#[macro_export]
macro_rules! sched_log {
    ($($arg:tt)*) => {{
        ::kernel_intf::debug!("[SCHED] {}", ::core::format_args!($($arg)*));
    }};
}

#[cfg(not(all(debug_assertions, feature = "debug-scheduler-logs")))]
#[macro_export]
macro_rules! sched_log {
    ($($arg:tt)*) => {};
}

#[cfg(all(debug_assertions, feature = "debug-mem-logs"))]
#[macro_export]
macro_rules! mem_log {
    ($($arg:tt)*) => {{
        ::kernel_intf::debug!("[MEM] {}", ::core::format_args!($($arg)*));
    }};
}

#[cfg(not(all(debug_assertions, feature = "debug-mem-logs")))]
#[macro_export]
macro_rules! mem_log {
    ($($arg:tt)*) => {};
}

#[cfg(all(debug_assertions, feature = "debug-allocator-logs"))]
#[macro_export]
macro_rules! allocator_log {
    ($($arg:tt)*) => {{
        ::kernel_intf::debug!("[ALLOCATOR] {}", ::core::format_args!($($arg)*));
    }};
}

#[cfg(not(all(debug_assertions, feature = "debug-allocator-logs")))]
#[macro_export]
macro_rules! allocator_log {
    ($($arg:tt)*) => {};
}

#[cfg(all(debug_assertions, feature = "debug-io-logs"))]
#[macro_export]
macro_rules! io_log {
    ($($arg:tt)*) => {{
        ::kernel_intf::debug!("[IO] {}", ::core::format_args!($($arg)*));
    }};
}

#[cfg(not(all(debug_assertions, feature = "debug-io-logs")))]
#[macro_export]
macro_rules! io_log {
    ($($arg:tt)*) => {};
}

#[cfg(all(debug_assertions, feature = "debug-loader-logs"))]
#[macro_export]
macro_rules! loader_log {
    ($($arg:tt)*) => {{
        ::kernel_intf::debug!("[LOADER] {}", ::core::format_args!($($arg)*));
    }};
}

#[cfg(not(all(debug_assertions, feature = "debug-loader-logs")))]
#[macro_export]
macro_rules! loader_log {
    ($($arg:tt)*) => {};
}
