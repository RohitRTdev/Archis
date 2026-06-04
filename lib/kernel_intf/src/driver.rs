use core::ffi::c_void;

use common::MemoryRegion;

use super::StrRef;

#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IrpMajor {
    Read = 0,
    Write = 1,
    Start = 2,
    Stop = 3,
    Configure = 4,
    Remove = 5
}

impl IrpMajor {
    pub fn from_usize(v: usize) -> Option<Self> {
        match v {
            0 => Some(Self::Read),
            1 => Some(Self::Write),
            2 => Some(Self::Start),
            3 => Some(Self::Stop),
            4 => Some(Self::Configure),
            5 => Some(Self::Remove),
            _ => None
        }
    }
}

#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IrpMinor {
    None = 0,
    Enumerate = 1,
    Query = 2
}

impl IrpMinor {
    pub fn from_usize(v: usize) -> Option<Self> {
        match v {
            0 => Some(Self::None),
            1 => Some(Self::Enumerate),
            2 => Some(Self::Query),
            _ => None
        }
    }
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
#[derive(Clone, Copy)]
pub struct ResourceList {
    irq: usize,
    ports: &'static [usize]
}

#[repr(C)]
pub union ReqInfo {
    pub start: ResourceList,
    pub enumerate: &'static [*const DeviceObject]
}

#[repr(C)]
pub struct Irp {
    pub major_code: IrpMajor,
    pub minor_code: IrpMinor,
    pub req_params: Option<ReqInfo>,
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
            req_params: None, 
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
            assert!(status == Status::Success || status == Status::Failed);
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
    pub dispatch_configure: Option<DeviceDispatch>,
    pub dispatch_remove: Option<DeviceDispatch>
}

impl DispatchTable {
    pub const fn new() -> Self {
        Self {
            dispatch_add: None,
            dispatch_read: None,
            dispatch_write: None,
            dispatch_start: None,
            dispatch_stop: None,
            dispatch_configure: None,
            dispatch_remove: None
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

    pub fn invoke_configure(&self, device: *const DeviceObject, irp: *mut Irp) -> Status {
        Self::invoke_device(self.dispatch_configure, device, irp)
    }

    pub fn invoke_remove(&self, device: *const DeviceObject, irp: *mut Irp) -> Status {
        Self::invoke_device(self.dispatch_remove, device, irp)
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
    pub id: usize,
    pub name: StrRef,
    pub ctx: *mut c_void
}

unsafe impl Sync for DeviceObject {}
unsafe impl Send for DeviceObject {}

impl DeviceObject {
    pub fn new(id: usize, name: Option<&'static str>, ctx: *mut c_void) -> Self {
        let str_ref = name.map_or(StrRef::from_str(""), StrRef::from_str);
        Self { id, name: str_ref, ctx }
    }

    pub fn get_name(&self) -> Option<&str> {
        let name = unsafe { self.name.as_str() };
        if name.is_empty() {
            None
        }
        else {
            Some(name)
        }
    }

    pub fn get_driver_id(&self) -> usize {
        unsafe { super::io_get_driver_id(self as *const _) }
    }
}

pub fn create_device_by_id(
    driver_id: usize,
    name: Option<&'static str>,
    ctx: *mut c_void,
    parent: Option<&DeviceObject>
) -> *mut DeviceObject {
    let name = name.map_or(StrRef::from_str(""), StrRef::from_str);
    let parent = parent.map(|p| p as *const DeviceObject).unwrap_or(core::ptr::null());
    unsafe { super::io_create_device(driver_id, name, ctx, parent) }
}

pub fn create_device(
    driver: &DriverObject,
    name: Option<&'static str>,
    ctx: *mut c_void,
    parent: Option<&DeviceObject>
) -> *mut DeviceObject {
    create_device_by_id(driver.id, name, ctx, parent)
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
