use core::ffi::c_void;

use common::MemoryRegion;
use crate::driver::IrpMajor::Read;

use super::StrRef;

#[repr(u64)]
pub enum IrpMajor {
    Read = 0,
    Write = 1,
    Add = 2,
    Start = 3,
    Enumerate = 4,
    Stop = 5
}

#[repr(u64)]
pub enum IrpMinor {
    None = 0
}

#[repr(C)]
pub struct Irp {
    major_code: IrpMajor,
    minor_code: IrpMinor,
    buffer: MemoryRegion,
    status: Status,
    completion_routine: Option<fn(*mut Irp)>
}

impl Irp {
    pub fn complete_irp(&mut self) {
        self.status = Status::Success;
        if let Some(f) = self.completion_routine {
            f(self as *mut _);
        };
    }

    pub fn new() -> Self {
        Self {
            major_code: IrpMajor::Read,
            minor_code: IrpMinor::None,
            buffer: MemoryRegion {base_address: 0, size: 0},
            status: Status::Pending,
            completion_routine: None
        }
    }
}

#[repr(i64)]
#[derive(PartialEq)]
pub enum Status {
    Success = 0,
    Pending = 1,
    Failed = -1
}

#[repr(C)]
pub struct DispatchTable {
    pub dispatch_add: Option<unsafe extern "C" fn(*const DeviceObject, *const Irp) -> Status>,
    pub dispatch_read: Option<unsafe extern "C" fn(*const DeviceObject, *const Irp) -> Status>,
    pub dispatch_write: Option<unsafe extern "C" fn(*const DeviceObject, *const Irp) -> Status>,
    pub dispatch_enumerate: Option<unsafe extern "C" fn(*const DeviceObject, *const Irp) -> Status>,
    pub dispatch_stop: Option<unsafe extern "C" fn(*const DeviceObject, *const Irp) -> Status>,
    pub dispatch_start: Option<unsafe extern "C" fn(*const DeviceObject, *const Irp) -> Status>,
}

impl DispatchTable {
    fn new() -> Self {
        Self {
            dispatch_add: None,
            dispatch_enumerate: None,
            dispatch_read: None,
            dispatch_start: None,
            dispatch_stop: None,
            dispatch_write: None
        }
    }

    pub fn read(&self, device: *const DeviceObject, req: *const Irp) {
        if let Some(read) = self.dispatch_read {
            unsafe { read(device, req); }
        }
    }
}

#[repr(C)]
pub struct DeviceObject {
    pub name: StrRef,
    pub ctx: *mut c_void
}

impl DeviceObject {
    pub fn new() -> Self {
        Self {
            name: StrRef::from_str("testing"),
            ctx: core::ptr::null_mut()
        }
    }
}

#[repr(C)]
pub struct DriverObject {
    name: StrRef,
    pub dispatch: DispatchTable
}

unsafe impl Sync for DriverObject{}
unsafe impl Send for DriverObject{}

impl DriverObject {
    pub fn new(name: &str) -> Self {
        Self {
            name: StrRef::from_str(name),
            dispatch: DispatchTable::new() 
        }
    }

    pub fn get_name(&self) -> &str {
        unsafe { self.name.as_str() }
    }
}
