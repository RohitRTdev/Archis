#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

extern crate alloc;

mod log;
pub use log::*;
pub mod mem;
pub mod ds;
pub use ds::list;
pub mod driver;
use core::{ffi::c_void, fmt};
use common::StrRef;
use alloc::vec::Vec;

pub type InterruptRoutine = extern "C" fn(*mut core::ffi::c_void) -> bool;
pub type SessionType      = usize;
pub type ProcessGroupType = usize;

pub const SIGINT:  u8 = 0;
pub const SIGFPE:  u8 = 1;
pub const SIGSEGV: u8 = 2;
pub const SIGILL:  u8 = 3;
pub const SIGKILL: u8 = 4;
pub const SIGTTIN: u8 = 5;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InterruptHandle {
    pub irq:      usize,
    pub node_ptr: usize
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KError {
    Success,
    InvalidArgument,
    OutOfMemory,
    ProcessTerminated,
    ProcessInitFailed,
    WaitFailed,
    WaitTimedOut,
    WaitInterrupted,
    CircularDependency,
    ModuleNotDriver,
    DriverLoadFailed,
    Unsupported,
    DeviceStopped,
    DeviceRemoved,
    DeviceStarted
}

pub const E_SUCCESS: i64 = 0;
pub const E_INVALID: i64 = -1;
pub const E_OOM: i64 = -2;
pub const E_INTERNAL_FAILURE: i64 = -3;
pub const E_NOT_SUPPORTED: i64 = -4;
pub const E_DEV_STOPPED: i64 = -5;
pub const E_INVALID_MEMORY_RANGE: i64 = -6;
pub const E_PROCESS_TERMINATED: i64 = -7;
pub const E_NOPERM: i64 = -8;
pub const E_DEV_REMOVED: i64 = -9;
pub const E_DEV_STARTED: i64 = -10;
pub const E_WAIT_INTERRUPTED: i64 = -11;
pub const E_TIMEOUT: i64 = -12;

impl<T> From<Result<T, KError>> for KError {
    fn from(e: Result<T, KError>) -> Self {
        e.err().unwrap_or(KError::Success)
    }
}

impl From<KError> for i64 {
    fn from(e: KError) -> Self {
        match e {
            KError::Success => E_SUCCESS,
            KError::InvalidArgument => E_INVALID,
            KError::OutOfMemory => E_OOM,
            KError::Unsupported => E_NOT_SUPPORTED,
            KError::DeviceStopped => E_DEV_STOPPED,
            KError::DeviceRemoved => E_DEV_REMOVED,
            KError::DeviceStarted => E_DEV_STARTED,
            KError::ProcessTerminated => E_PROCESS_TERMINATED,
            KError::WaitInterrupted => E_WAIT_INTERRUPTED,
            KError::WaitTimedOut => E_TIMEOUT,
            KError::WaitFailed | KError::CircularDependency | KError::DriverLoadFailed |
            KError::ModuleNotDriver | KError::ProcessInitFailed => E_INTERNAL_FAILURE
        }
    }
}


impl fmt::Display for KError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let description = match self {
            KError::InvalidArgument => "Invalid argument",
            KError::OutOfMemory => "Out of memory",
            KError::ProcessTerminated => "Process terminated",
            KError::ProcessInitFailed => "Process init failed",
            KError::WaitFailed => "Wait failed",
            KError::WaitTimedOut => "Wait timed out",
            KError::WaitInterrupted => "Wait interrupted",
            KError::CircularDependency => "Circular dependency in module load",
            KError::DriverLoadFailed => "Driver load failed",
            KError::Unsupported => "Operation not supported",
            KError::ModuleNotDriver => "Loaded module is not a driver",
            KError::DeviceStopped => "Device is stopped",
            KError::DeviceRemoved => "Device has been removed",
            KError::DeviceStarted => "Device is already started",
            KError::Success => "Success"
        };
        write!(f, "{}", description)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RtcTime {
    pub second: u8,
    pub minute: u8,
    pub hour: u8,
    pub day: u8,
    pub month: u8,
    pub year: u8
}

impl fmt::Display for RtcTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02}/{:02}/{:02}:{:02}-{:02}-{:02}",
            self.day, self.month, self.year, self.hour, self.minute, self.second
        )
    }
}

#[macro_export]
macro_rules! get_module_name {
    () => {
        MODULE_NAME_STR
    }
}

#[repr(C)]
pub struct Lock {
    pub lock: u64,
    pub int_status: bool
}

#[cfg_attr(not(feature = "link-kernel"), link(name = "aris"))]
unsafe extern "C" {
    fn create_spinlock_ffi(lock: &mut Lock);
    fn acquire_spinlock_ffi(lock: &mut Lock);
    fn release_spinlock_ffi(lock: &mut Lock);
    pub fn clear_screen();
    pub fn read_rtc() -> RtcTime;
    pub fn read_timestamp() -> usize;
    pub fn get_core_ffi() -> usize;
    pub fn serial_print_ffi(s: *const u8, len: usize);
    pub fn map_memory_ffi(phys_addr: usize, phys_addr: usize, size: usize, flags: u8) -> KError;
    pub fn unmap_memory_ffi(virt_addr: *mut u8, size: usize) -> KError; 
    pub fn allocate_memory_ffi(size: usize, align: usize, flags: u8) -> KError;
    pub fn deallocate_memory_ffi(addr: *mut u8, size: usize, align: usize, flags: u8) -> KError;
    pub fn pool_alloc_ffi(size: usize, align: usize, out: *mut *mut u8) -> KError;
    pub fn pool_dealloc_ffi(ptr: *mut u8, size: usize, align: usize) -> KError;
    pub fn heap_alloc_ffi(size: usize, align: usize, out: *mut *mut u8) -> KError;
    pub fn heap_dealloc_ffi(ptr: *mut u8, size: usize, align: usize) -> KError;
    pub fn panic_router(mod_name: StrRef, info: StrRef) -> !;
    pub fn io_create_device(
        driver_id: usize,
        name: StrRef,
        ctx: *mut core::ffi::c_void,
        parent: *const driver::DeviceObject,
        is_class: bool
    ) -> *mut driver::DeviceObject;

    fn io_send_request_ffi(
        device: *const driver::DeviceObject,
        major: usize,
        minor: usize,
        buf_base: usize,
        buf_size: usize,
        offset: usize,
        req_info_ptr: *const driver::ReqInfo,
        completion: Option<extern "C" fn(*const driver::IrpResult, *mut core::ffi::c_void)>,
        completion_ctx: *mut core::ffi::c_void
    ) -> driver::Status;

    fn tty_print_ffi(s: *const u8, len: usize);
    fn enable_tty_mode_ffi();
    fn disable_tty_mode_ffi();

    fn io_complete_irp_ffi(irp: *mut driver::Irp, status: driver::Status);
    pub fn io_get_driver_id(device: *const driver::DeviceObject) -> usize;
    fn io_start_device_ffi(device: *const driver::DeviceObject) -> driver::Status;
    fn io_stop_device_ffi(device: *const driver::DeviceObject) -> driver::Status;
    fn io_remove_device_ffi(device: *const driver::DeviceObject) -> driver::Status;
    pub fn io_invalidate_device(device: *const driver::DeviceObject) -> driver::Status;
    fn io_set_cancel_routine_ffi(
        irp: *mut driver::Irp,
        routine: extern "C" fn(*const driver::DeviceObject, *mut driver::Irp)
    ); 
    fn io_start_processing_ffi(irp: *mut driver::Irp) -> bool;

    fn sched_delay_ms_ffi(value: usize);
    fn io_install_interrupt_handler_ffi(
        irq: usize,
        context: *mut core::ffi::c_void,
        handler: InterruptRoutine,
        active_high: bool,
        is_edge_triggered: bool
    ) -> InterruptHandle;

    fn io_remove_interrupt_handler_ffi(handle: InterruptHandle);
    fn sched_exit_process_ffi(exit_code: isize) -> !;
    fn sched_get_num_process_args_ffi() -> usize;
    fn sched_get_cur_process_arg_ffi(num: usize) -> StrRef;
    fn sched_get_cur_thread_arg_ffi() -> *mut c_void;
    fn sched_get_cur_thread_id_ffi() -> usize;

    fn sched_create_process_ffi(args: *const StrRef, args_len: usize, context_ptr: *mut c_void) -> usize;
    fn sched_get_current_pid_ffi() -> isize;
    fn sched_wait_process_ffi(proc_id: usize);
    fn sched_kill_process_ffi(proc_id: usize, exit_code: isize);
    fn sched_create_thread_ffi(handler: extern "C" fn() -> !, context_ptr: *mut c_void) -> usize;
    fn sched_exit_thread_ffi(exit_code: isize) -> !;
    fn sched_kill_thread_ffi(thread_id: usize, exit_code: isize);

    fn io_create_driver_worker_ffi(
        routine: extern "C" fn(*mut c_void),
        context: *mut c_void,
    ) -> KError;

    fn proc_get_session_ffi(pid: usize) -> SessionType;
    fn proc_drop_session_ffi(val: SessionType);
    fn proc_is_session_active_ffi(val: SessionType) -> bool;
    fn proc_is_session_leader_ffi(pid: usize, val: SessionType) -> bool;
    fn proc_get_pgrp_ffi(pid: usize) -> ProcessGroupType;
    fn proc_drop_pgrp_ffi(val: ProcessGroupType);
    fn proc_is_pgrp_active_ffi(val: ProcessGroupType) -> bool;
    fn proc_is_foreground_pgrp_ffi(pid: usize, val: ProcessGroupType) -> bool;
    fn proc_issue_signal_ffi(pid: usize, signal: u8);
    fn proc_issue_pgrp_ffi(val: ProcessGroupType, signal: u8);
}

pub fn io_create_driver_worker(
    routine: extern "C" fn(*mut c_void),
    context: *mut c_void,
) -> Result<(), KError> {
    let res = unsafe { io_create_driver_worker_ffi(routine, context) };
    match res {
        KError::Success => Ok(()),
        e => Err(e),
    }
}

pub fn sched_delay_ms(value: usize) {
    unsafe { sched_delay_ms_ffi(value); }
}

pub fn io_start_processing(irp: *mut driver::Irp) -> bool {
    unsafe { io_start_processing_ffi(irp) }
}

pub fn io_complete_irp(irp: *mut driver::Irp, status: driver::Status) {
    unsafe { io_complete_irp_ffi(irp, status) }
}

pub fn io_install_interrupt_handler(
    irq: usize,
    context: *mut core::ffi::c_void,
    handler: InterruptRoutine,
    active_high: bool,
    is_edge_triggered: bool,
) -> InterruptHandle {
    unsafe { io_install_interrupt_handler_ffi(irq, context, handler, active_high, is_edge_triggered) }
}

pub fn io_remove_interrupt_handler(handle: InterruptHandle) {
    unsafe { io_remove_interrupt_handler_ffi(handle); }
}

pub fn io_set_cancel_routine(
    irp: &mut driver::Irp,
    routine: extern "C" fn(*const driver::DeviceObject, *mut driver::Irp)
) {
    unsafe { io_set_cancel_routine_ffi(irp, routine); }
}

pub fn sched_exit_process(exit_code: isize) -> ! {
    unsafe { sched_exit_process_ffi(exit_code) }
}

pub fn sched_get_num_process_args() -> usize {
    unsafe { sched_get_num_process_args_ffi() }
}

pub fn sched_get_cur_process_arg(num: usize) -> StrRef {
    unsafe { sched_get_cur_process_arg_ffi(num) }
}

pub fn sched_get_cur_thread_arg() -> *mut c_void {
    unsafe { sched_get_cur_thread_arg_ffi() }
}

pub fn sched_get_cur_thread_id() -> usize {
    unsafe { sched_get_cur_thread_id_ffi() }
}

pub fn sched_create_process(args: &[&str], context_ptr: *mut c_void) -> Option<usize> {
    let refs: Vec<StrRef> = args.iter().map(|s| StrRef::from_str(s)).collect();
    let res = unsafe { sched_create_process_ffi(refs.as_ptr(), refs.len(), context_ptr) };
    if res == usize::MAX {
        None
    }
    else {
        Some(res)
    }
}

pub fn sched_get_current_pid() -> isize {
    unsafe { sched_get_current_pid_ffi() }
}

pub fn sched_wait_process(proc_id: usize) {
    unsafe { sched_wait_process_ffi(proc_id) }
}

pub fn sched_kill_process(proc_id: usize, exit_code: isize) {
    unsafe { sched_kill_process_ffi(proc_id, exit_code) }
}

pub fn sched_create_thread(handler: extern "C" fn() -> !, context_ptr: *mut c_void) -> Option<usize> {
    let res = unsafe { sched_create_thread_ffi(handler, context_ptr) };
    if res == usize::MAX {
        None
    }
    else {
        Some(res)
    }
}

pub fn sched_exit_thread(exit_code: isize) -> ! {
    unsafe { sched_exit_thread_ffi(exit_code) }
}

pub fn sched_kill_thread(thread_id: usize, exit_code: isize) {
    unsafe { sched_kill_thread_ffi(thread_id, exit_code) }
}

pub fn create_spinlock(lock: &mut Lock) {
    unsafe { create_spinlock_ffi(lock) }
}

pub fn acquire_spinlock(lock: &mut Lock) {
    unsafe { acquire_spinlock_ffi(lock); }
} 

pub fn release_spinlock(lock: &mut Lock) {
    unsafe { release_spinlock_ffi(lock); }
}

pub fn io_send_request(
    device: *const driver::DeviceObject,
    major: usize,
    minor: usize,
    buf_base: usize,
    buf_size: usize,
    offset: usize,
    req_info_ptr: *const driver::ReqInfo,
    completion: Option<extern "C" fn(*const driver::IrpResult, *mut core::ffi::c_void)>,
    completion_ctx: *mut core::ffi::c_void
) -> driver::Status {
    unsafe { io_send_request_ffi(device, major, minor, buf_base, buf_size, offset, req_info_ptr, completion, completion_ctx) }
}

pub fn io_start_device(device: *const driver::DeviceObject) -> driver::Status {
    unsafe { io_start_device_ffi(device) }
}

pub fn io_stop_device(device: *const driver::DeviceObject) -> driver::Status {
    unsafe { io_stop_device_ffi(device) }
}

pub fn io_remove_device(device: *const driver::DeviceObject) -> driver::Status {
    unsafe { io_remove_device_ffi(device) }
}

pub fn enable_tty_mode() {
    unsafe { enable_tty_mode_ffi(); }
}

pub fn disable_tty_mode() {
    unsafe { disable_tty_mode_ffi(); }
}

pub fn tty_print(s: &str) {
    unsafe { tty_print_ffi(s.as_ptr(), s.len()); }
}

pub fn proc_get_session(pid: usize) -> SessionType {
    unsafe { proc_get_session_ffi(pid) }
}

pub fn proc_drop_session(val: SessionType) {
    unsafe { proc_drop_session_ffi(val) }
}

pub fn proc_is_session_active(val: SessionType) -> bool {
    unsafe { proc_is_session_active_ffi(val) }
}

pub fn proc_is_session_leader(pid: usize, val: SessionType) -> bool {
    unsafe { proc_is_session_leader_ffi(pid, val) }
}

pub fn proc_get_pgrp(pid: usize) -> ProcessGroupType {
    unsafe { proc_get_pgrp_ffi(pid) }
}

pub fn proc_drop_pgrp(val: ProcessGroupType) {
    unsafe { proc_drop_pgrp_ffi(val) }
}

pub fn proc_is_pgrp_active(val: ProcessGroupType) -> bool {
    unsafe { proc_is_pgrp_active_ffi(val) }
}

pub fn proc_is_foreground_pgrp(pid: usize, val: ProcessGroupType) -> bool {
    unsafe { proc_is_foreground_pgrp_ffi(pid, val) }
}

pub fn proc_issue_signal(pid: usize, signal: u8) {
    unsafe { proc_issue_signal_ffi(pid, signal) }
}

pub fn proc_issue_pgrp(val: ProcessGroupType, signal: u8) {
    unsafe { proc_issue_pgrp_ffi(val, signal) }
}

#[macro_export]
macro_rules! run_tests {
    () => {
        #[cfg(feature = "kunit-test")]
        {
            unsafe extern "C" {
                static __kunit_tests_start: $crate::kunit::KUnitEntry;
                static __kunit_tests_end: $crate::kunit::KUnitEntry;
            }
            $crate::kunit::_run_range(
                unsafe { &__kunit_tests_start as *const $crate::kunit::KUnitEntry },
                unsafe { &__kunit_tests_end as *const $crate::kunit::KUnitEntry }
            );
        }
    }
}

#[cfg(feature = "kunit-test")]
pub mod kunit;
