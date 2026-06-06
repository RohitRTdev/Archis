#![no_std]
#![feature(allocator_api)]

mod log;
pub use log::*;
pub mod mem;
pub mod list;
pub mod driver;
use core::fmt;
use common::StrRef;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KError {
    Success,
    InvalidArgument,
    OutOfMemory,
    ProcessTerminated,
    WaitFailed,
    CircularDependency,
    DriverLoadFailed,
    Unsupported,
    DeviceStopped
}

pub const E_SUCCESS: i64 = 0;
pub const E_INVALID: i64 = -1;
pub const E_OOM: i64 = -2;
pub const E_INTERNAL_FAILURE: i64 = -3;
pub const E_NOT_SUPPORTED: i64 = -4;
pub const E_DEV_STOPPED: i64 = -5;

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
            KError::ProcessTerminated | KError::WaitFailed |
            KError::CircularDependency | KError::DriverLoadFailed => E_INTERNAL_FAILURE
        }
    }
}


impl fmt::Display for KError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let description = match self {
            KError::InvalidArgument => "Invalid argument",
            KError::OutOfMemory => "Out of memory",
            KError::ProcessTerminated => "Process terminated",
            KError::WaitFailed => "Wait internal failure",
            KError::CircularDependency => "Circular dependency in module load",
            KError::DriverLoadFailed => "Driver load failed",
            KError::Unsupported => "Operation not supported",
            KError::DeviceStopped => "Device is stopped",
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

#[repr(C)]
pub struct Lock {
    pub lock: u64,
    pub int_status: bool
}

#[cfg_attr(not(feature = "link-kernel"), link(name = "aris"))]
unsafe extern "C" {
    pub fn create_spinlock(lock: &mut Lock);
    pub fn acquire_spinlock(lock: &mut Lock);
    pub fn release_spinlock(lock: &mut Lock);
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
    pub fn exported_function();
    pub fn io_create_device(
        driver_id: usize, 
        name: StrRef, 
        ctx: *mut core::ffi::c_void, 
        parent: *const driver::DeviceObject) 
    -> *mut driver::DeviceObject;

    pub fn io_send_request(
        device: *const driver::DeviceObject,
        major: usize,
        minor: usize,
        buf_base: usize,
        buf_size: usize,
        offset: usize,
        completion: Option<extern "C" fn(*const driver::IrpResult, *mut core::ffi::c_void)>,
        completion_ctx: *mut core::ffi::c_void
    ) -> driver::Status;
    
    pub fn io_complete_irp(irp: *mut driver::Irp, status: driver::Status);
    pub fn io_get_driver_id(device: *const driver::DeviceObject) -> usize;
    pub fn io_invalidate_device(device: *const driver::DeviceObject) -> driver::Status;
    pub fn io_set_cancel_routine(
        irp: *mut driver::Irp,
        routine: extern "C" fn(*const driver::DeviceObject, *mut driver::Irp)
    ); 
    pub fn io_start_processing(irp: *mut driver::Irp) -> bool;

    #[allow(improper_ctypes)]
    pub fn create_kernel_thread(handler: fn() -> !) -> KError;
    pub fn exit_kernel_thread() -> !;
    pub fn delay_ms_ffi(value: usize);
    pub fn install_interrupt_handler_ffi(
        irq: usize,
        handler: extern "C" fn(usize),
        active_high: bool,
        is_edge_triggered: bool,
    );
}

