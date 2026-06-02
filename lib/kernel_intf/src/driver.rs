use core::ffi::c_void;

use common::MemoryRegion;

use super::StrRef;

#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IrpMajor {
    Read = 0,
    Write = 1,
    Add = 2,
    Start = 3,
    Enumerate = 4,
    Stop = 5
}

#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IrpMinor {
    None = 0
}

#[repr(isize)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Success = 0,
    Pending = 1,
    Failed = -1,
    Unsupported = -2
}

#[repr(C)]
pub struct Irp {
    pub major_code: IrpMajor,
    pub minor_code: IrpMinor,
    pub buffer: MemoryRegion,
    pub offset: usize,
    pub status: Status,
    pub bytes_completed: usize,
    pub completion_routine: Option<extern "C" fn(*mut Irp, *mut c_void)>,
    pub completion_ctx: *mut c_void
}

impl Irp {
    pub fn new(
        major_code: IrpMajor,
        buffer: MemoryRegion,
        offset: usize,
        completion_routine: Option<extern "C" fn(*mut Irp, *mut c_void)>,
        completion_ctx: *mut c_void
    ) -> Self {
        Self {
            major_code,
            minor_code: IrpMinor::None,
            buffer,
            offset,
            status: Status::Pending,
            bytes_completed: 0,
            completion_routine,
            completion_ctx
        }
    }

    pub fn complete_irp(&mut self, status: Status) {
        if let Some(routine) = self.completion_routine {
            self.status = status;
            routine(self as *mut _, self.completion_ctx);
        }
    }
}

pub type DeviceDispatch = unsafe extern "C" fn(*const DeviceObject, *mut Irp) -> Status;
pub type AddDispatch = unsafe extern "C" fn(*const DriverObject, *const DeviceObject) -> Status;

#[repr(C)]
pub struct DispatchTable {
    pub dispatch_add: Option<AddDispatch>,
    pub dispatch_read: Option<DeviceDispatch>,
    pub dispatch_write: Option<DeviceDispatch>,
    pub dispatch_start: Option<DeviceDispatch>,
    pub dispatch_stop: Option<DeviceDispatch>,
    pub dispatch_enumerate: Option<DeviceDispatch>
}

impl DispatchTable {
    pub const fn new() -> Self {
        Self {
            dispatch_add: None,
            dispatch_read: None,
            dispatch_write: None,
            dispatch_start: None,
            dispatch_stop: None,
            dispatch_enumerate: None
        }
    }

    fn invoke_device(
        handler: Option<DeviceDispatch>,
        device: *const DeviceObject,
        irp: *mut Irp
    ) -> Status {
        match handler {
            Some(handler) => unsafe { handler(device, irp) },
            None => Status::Unsupported
        }
    }

    pub fn invoke_read(&self, device: *const DeviceObject, irp: *mut Irp) -> Status {
        Self::invoke_device(self.dispatch_read, device, irp)
    }

    pub fn invoke_write(&self, device: *const DeviceObject, irp: *mut Irp) -> Status {
        Self::invoke_device(self.dispatch_write, device, irp)
    }

    pub fn invoke_start(&self, device: *const DeviceObject, irp: *mut Irp) -> Status {
        Self::invoke_device(self.dispatch_start, device, irp)
    }

    pub fn invoke_stop(&self, device: *const DeviceObject, irp: *mut Irp) -> Status {
        Self::invoke_device(self.dispatch_stop, device, irp)
    }

    pub fn invoke_enumerate(&self, device: *const DeviceObject, irp: *mut Irp) -> Status {
        Self::invoke_device(self.dispatch_enumerate, device, irp)
    }

    pub fn invoke_add(&self, driver: *const DriverObject, pdo: *const DeviceObject) -> Status {
        match self.dispatch_add {
            Some(handler) => unsafe { handler(driver, pdo) },
            None => Status::Unsupported
        }
    }
}

impl Default for DispatchTable {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C)]
pub struct DeviceObject {
    pub name: StrRef,
    pub ctx: *mut c_void
}

unsafe impl Sync for DeviceObject {}
unsafe impl Send for DeviceObject {}

impl DeviceObject {
    pub fn new(name: StrRef, ctx: *mut c_void) -> Self {
        Self { name, ctx }
    }

    pub fn get_name(&self) -> &str {
        unsafe { self.name.as_str() }
    }
}

#[repr(C)]
pub struct DriverObject {
    pub id: usize,
    name: StrRef,
    pub dispatch: DispatchTable
}

unsafe impl Sync for DriverObject {}
unsafe impl Send for DriverObject {}

impl DriverObject {
    pub fn new(id: usize, name: StrRef) -> Self {
        Self {
            id,
            name,
            dispatch: DispatchTable::new()
        }
    }

    pub fn get_name(&self) -> &str {
        unsafe { self.name.as_str() }
    }
}
