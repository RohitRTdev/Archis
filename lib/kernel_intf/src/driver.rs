use core::ffi::c_void;
use common::MemoryRegion;
use crate::io_complete_irp;
use super::{Lock, StrRef};

pub const EMPTY_REGION: MemoryRegion = MemoryRegion { base_address: 0, size: 0 };
pub const MAX_RESOURCE_ENTRIES: usize = 16;

#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IrpMajor {
    Read    = 0,
    Write   = 1,
    Pnp     = 2,
    Control = 3,
    Open    = 4,
    Close   = 5,
}

impl IrpMajor {
    pub fn from_usize(v: usize) -> Option<Self> {
        match v {
            0 => Some(Self::Read),
            1 => Some(Self::Write),
            2 => Some(Self::Pnp),
            3 => Some(Self::Control),
            4 => Some(Self::Open),
            5 => Some(Self::Close),
            _ => None
        }
    }
}

#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IrpMinor {
    None                    = 0,
    Enumerate               = 1,
    Query                   = 2,
    Resources               = 3,
    Start                   = 4,
    Stop                    = 5,
    Remove                  = 6,
    RegisterKeyboardHandler = 7,
    SetForegroundPgrp       = 8,
    SetControllingTty       = 9
}

impl IrpMinor {
    pub fn from_usize(v: usize) -> Option<Self> {
        match v {
            0 => Some(Self::None),
            1 => Some(Self::Enumerate),
            2 => Some(Self::Query),
            3 => Some(Self::Resources),
            4 => Some(Self::Start),
            5 => Some(Self::Stop),
            6 => Some(Self::Remove),
            7 => Some(Self::RegisterKeyboardHandler),
            8 => Some(Self::SetForegroundPgrp),
            9 => Some(Self::SetControllingTty),
            _ => None
        }
    }
}

#[repr(isize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Success     =  0,
    Pending     =  1,
    Failed      = -1,
    Unsupported = -2,
    Cancelled   = -3
}

// Raw keystroke produced by port drivers and forwarded to the input driver.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Keystroke {
    pub scancode: u8,
    pub ascii:    u8,  // 0 if modifier / unmapped key
    pub flags:    u8   // bit 0 = key-release
}

pub type KeystrokeHandler = unsafe extern "C" fn(*const Keystroke, count: usize, ctx: *mut c_void);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RegisterHandlerInfo {
    pub handler: Option<KeystrokeHandler>,
    pub context: *mut c_void,
}

impl RegisterHandlerInfo {
    pub const fn new() -> Self {
        Self {
            handler: None,
            context: core::ptr::null_mut()
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TtyControlInfo {
    pub pid:   usize
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntDesc {
    pub irq: usize,
    pub vector: usize,
    pub active_high: bool,
    pub edge_triggered: bool
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IrqInfo {
    pub gsi: u32,
    pub active_high: bool,
    pub edge_triggered: bool
}

#[repr(usize)]
#[derive(PartialEq, Clone, Copy)]
pub enum ResType {
    Memory,
    Port,
    Interrupt
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PortDesc {
    pub base: usize,
    pub range: usize
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union ResTypeDesc {
    pub mem: MemoryRegion,
    pub port: PortDesc,
    pub interrupt: IntDesc
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ResEntry {
    pub res_type: ResType, 
    pub desc: ResTypeDesc
}

impl Default for ResEntry {
    fn default() -> Self {
        Self {
            res_type: ResType::Memory,
            desc: ResTypeDesc {
                mem: MemoryRegion { base_address: 0, size: 0 }
            }
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ResList {
    pub base: *mut ResEntry,
    pub count: usize
}

// Request-specific parameters placed on the IRP before dispatch.
// Drivers read the relevant variant based on the major/minor code they handle.
#[repr(C)]
#[derive(Clone, Copy)]
pub union ReqInfo {
    pub _unused:          [usize; 2],
    pub register_handler: RegisterHandlerInfo,
    pub tty_control:      TtyControlInfo,
    pub res_list:         ResList
}

#[repr(C)]
pub struct Irp {
    pub major_code: IrpMajor,
    pub minor_code: IrpMinor,
    pub buffer: MemoryRegion,
    pub offset: usize,
    pub req_info: ReqInfo,
    pub status: Status,
    pub bytes_completed: usize,
    pub completion_routine: extern "C" fn(*mut Irp, *mut c_void),
    pub completion_ctx: *mut c_void,

    // Kernel accounting; drivers do not read these.
    pub device: usize,
    pub is_cancelled: bool,
    pub cancel_routine: Option<extern "C" fn(*const DeviceObject, *mut Irp)>,
    pub cancel_lock: Lock,
    pub thread_id: usize,
    pub is_completed: bool
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct IrpResult {
    pub major_code: IrpMajor,
    pub minor_code: IrpMinor,
    pub buffer: MemoryRegion,
    pub offset: usize,
    pub status: Status,
    pub bytes_completed: usize,
    pub req_info: ReqInfo
}

impl Default for IrpResult {
    fn default() -> Self {
        Self {
            major_code: IrpMajor::Read,
            minor_code: IrpMinor::None,
            buffer: EMPTY_REGION,
            offset: 0,
            status: Status::Pending,
            bytes_completed: 0,
            req_info: unsafe { core::mem::zeroed() }
        }
    }
}

impl Irp {
    pub fn new(
        major_code: IrpMajor,
        buffer: MemoryRegion,
        offset: usize,
        completion_routine: extern "C" fn(*mut Irp, *mut c_void),
        completion_ctx: *mut c_void,
        device: usize,
        thread_id: usize
    ) -> Self {
        let mut cancel_lock = Lock { lock: 0, int_status: false };
        super::create_spinlock(&mut cancel_lock);
        Self {
            major_code,
            minor_code: IrpMinor::None,
            buffer,
            offset,
            req_info: unsafe { core::mem::zeroed() },
            status: Status::Pending,
            bytes_completed: 0,
            completion_routine,
            completion_ctx,
            device,
            is_cancelled: false,
            cancel_routine: None,
            cancel_lock,
            thread_id,
            is_completed: false
        }
    }

    pub fn to_result(&self) -> IrpResult {
        IrpResult {
            major_code: self.major_code,
            minor_code: self.minor_code,
            buffer: self.buffer,
            offset: self.offset,
            status: self.status,
            bytes_completed: self.bytes_completed,
            req_info: self.req_info
        }
    }

    pub fn complete_irp(&mut self, status: Status) {
        io_complete_irp(self as *mut Irp, status);
    }
}

pub type DeviceDispatch = unsafe extern "C" fn(*const DeviceObject, *mut Irp) -> Status;
pub type AddDispatch = unsafe extern "C" fn(*const DriverObject, *const DeviceObject) -> Status;

#[repr(C)]
pub struct DispatchTable {
    pub dispatch_add:     Option<AddDispatch>,
    pub dispatch_read:    Option<DeviceDispatch>,
    pub dispatch_write:   Option<DeviceDispatch>,
    pub dispatch_pnp:     Option<DeviceDispatch>,
    pub dispatch_control: Option<DeviceDispatch>,
    pub dispatch_open:    Option<DeviceDispatch>,
    pub dispatch_close:   Option<DeviceDispatch>,
}

impl DispatchTable {
    pub const fn new() -> Self {
        Self {
            dispatch_add:     None,
            dispatch_read:    None,
            dispatch_write:   None,
            dispatch_pnp:     None,
            dispatch_control: None,
            dispatch_open:    None,
            dispatch_close:   None,
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

    pub fn invoke_pnp(&self, device: *const DeviceObject, irp: *mut Irp) -> Status {
        Self::invoke_device(self.dispatch_pnp, device, irp)
    }

    pub fn invoke_control(&self, device: *const DeviceObject, irp: *mut Irp) -> Status {
        Self::invoke_device(self.dispatch_control, device, irp)
    }

    pub fn invoke_open(&self, device: *const DeviceObject, irp: *mut Irp) -> Status {
        Self::invoke_device(self.dispatch_open, device, irp)
    }

    pub fn invoke_close(&self, device: *const DeviceObject, irp: *mut Irp) -> Status {
        Self::invoke_device(self.dispatch_close, device, irp)
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
    parent: Option<&DeviceObject>,
    is_class: bool
) -> *mut DeviceObject {
    let name = name.map_or(StrRef::from_str(""), StrRef::from_str);
    let parent = parent.map(|p| p as *const DeviceObject).unwrap_or(core::ptr::null());
    unsafe { super::io_create_device(driver_id, name, ctx, parent, is_class) }
}

pub fn create_device(
    driver: &DriverObject,
    name: Option<&'static str>,
    ctx: *mut c_void,
    parent: Option<&DeviceObject>,
    is_class: bool
) -> *mut DeviceObject {
    create_device_by_id(driver.id, name, ctx, parent, is_class)
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
