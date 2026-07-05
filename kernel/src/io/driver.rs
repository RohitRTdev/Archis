use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::cell::UnsafeCell;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use common::{MemoryRegion, StrRef};
use kernel_intf::driver::{
    DeviceObject, DriverObject, EMPTY_REGION, Irp, IrpMajor, IrpMinor, IrpResult, ReqInfo, ResList, Status
};
use kernel_intf::{acquire_spinlock, io_complete_irp, release_spinlock};
use kernel_intf::list::{DynList, List};
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::{KError, info};

use crate::io::stack::deallocate_device_resources;
use crate::loader::{LoadedImage, load_image};
use crate::sched::{self, cancel_irp, AsyncCtx, allocate_irp, disable_preemption, enable_preemption};
use crate::sync::{ConfigGuard, KEvent, KSem, Once, Spinlock, semaphore_guard};
use super::stack::{self, DeviceStack, LevelState};
pub type DriverHandle = Arc<DriverObjectK, PoolAllocatorGlobal>;
pub type DeviceHandleK = Arc<DeviceObjectK, PoolAllocatorGlobal>;
pub type IrpPtr = *mut Irp;

static NEXT_DRIVER_ID: AtomicUsize = AtomicUsize::new(0);
static NEXT_DEVICE_ID: AtomicUsize = AtomicUsize::new(0);
static DRIVER_REGISTRY: Spinlock<BTreeMap<usize, DriverHandle>> = Spinlock::new(BTreeMap::new());
static DRIVER_BY_NAME: Spinlock<BTreeMap<String, DriverHandle>> = Spinlock::new(BTreeMap::new());
static DEVICE_REGISTRY: Spinlock<BTreeMap<usize, DeviceHandleK>> = Spinlock::new(BTreeMap::new());
static DEVICE_BY_NAME: Spinlock<BTreeMap<String, DeviceHandleK>> = Spinlock::new(BTreeMap::new());
static ROOT_DEVICE: Once<DeviceHandleK> = Once::new();
static DRIVER_LOAD_LOCK: Once<KSem> = Once::new();

pub struct OpenDeviceHandleInner {
    dev: DeviceHandleK
}

impl Drop for OpenDeviceHandleInner {
    fn drop(&mut self) {
        match self.dev.state() {
            DeviceState::Stopping | DeviceState::Stopped |
            DeviceState::Removing | DeviceState::Removed => {
                return;
            },
            _ => {}
        }
        let _ = io_request_sync(&self.dev, IrpMajor::Close, IrpMinor::None, EMPTY_REGION, 0, None, false);
    }
}

impl core::ops::Deref for OpenDeviceHandleInner {
    type Target = DeviceObjectK;
    fn deref(&self) -> &Self::Target { &self.dev }
}

pub type OpenDeviceHandle = Arc<OpenDeviceHandleInner, PoolAllocatorGlobal>;

#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceState {
    Stopped  = 0,
    Started  = 1,
    Stopping = 2,
    Removing = 3,
    Removed  = 4
}

#[repr(usize)]
#[derive(PartialEq)]
enum DriverState {
    Loaded = 0,
    Unloaded = 1
}

pub struct DriverObjectK {
    image: UnsafeCell<Option<LoadedImage>>,
    state: AtomicUsize,
    driver: UnsafeCell<DriverObject>,
    driver_guard: KSem,
    devices: Spinlock<Vec<usize>>
}

unsafe impl Sync for DriverObjectK {} 

impl DriverObjectK {
    fn state(&self) -> DriverState {
        unsafe { core::mem::transmute(self.state.load(Ordering::Acquire)) }
    }

    fn set_state(&self, s: DriverState) {
        self.state.store(s as usize, Ordering::Release);
    }
}

pub struct DeviceObjectK {
    id: usize,
    device: DeviceObject,
    // This is only None for root device
    driver: Option<DriverHandle>,
    state: AtomicUsize,
    is_pdo: AtomicBool,
    // Class devices have a driver-managed lifecycle; they are never touched by PnP.
    is_class_device: bool,
    enabled: AtomicBool,
    parent: Spinlock<Option<usize>>,
    children: Spinlock<Vec<usize>>,
    stack: Spinlock<Option<(Arc<DeviceStack>, usize)>>,
    // Serializes start/stop/query/enumerate/remove/add for this device.
    // Read/write mutual exclusion is left to driver writer.
    config_sem: KSem,
    pending_irps: Spinlock<DynList<IrpPtr>>,
    resource_list: Spinlock<Option<ResList>>,
    request_lock: KSem,
    // Signalled whenever pending_irps becomes empty
    pending_irps_drained_event: KEvent
}

unsafe impl Send for DeviceObjectK {}
unsafe impl Sync for DeviceObjectK {}

impl DeviceObjectK {
    pub fn id(&self) -> usize {
        self.id
    }

    pub fn device_ptr(&self) -> *const DeviceObject {
        &self.device as *const DeviceObject
    }

    pub fn name(&self) -> &str {
        self.device.get_name().unwrap_or("<unnamed>")
    }

    pub fn get_pending_irps(&self) -> &Spinlock<DynList<IrpPtr>> {
        &self.pending_irps
    }

    pub fn pending_irps_drained_event(&self) -> &KEvent {
        &self.pending_irps_drained_event
    }

    pub fn is_class_device(&self) -> bool {
        self.is_class_device
    }

    pub fn is_pdo(&self) -> bool {
        self.is_pdo.load(Ordering::Acquire)
    }

    pub fn parent_id(&self) -> Option<usize> {
        *self.parent.lock()
    }

    pub fn set_parent(&self, pid: usize) {
        *self.parent.lock() = Some(pid);
    }

    pub fn state(&self) -> DeviceState {
        unsafe { core::mem::transmute(self.state.load(Ordering::Acquire)) }
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    fn set_state(&self, s: DeviceState) {
        self.state.store(s as usize, Ordering::Release);
    }

    fn is_started(&self) -> bool {
        self.state() == DeviceState::Started
    }

    pub fn config_guard(&self) -> ConfigGuard<'_> {
        semaphore_guard(&self.config_sem)
    }

    pub fn children_snapshot(&self) -> Vec<usize> {
        self.children.lock().clone()
    }

    // Child ids present now that were not in before 
    pub fn children_added(&self, before: &[usize]) -> Vec<usize> {
        self.children
            .lock()
            .iter()
            .copied()
            .filter(|id| !before.contains(id))
            .collect()
    }

    pub fn set_stack(&self, stack: Arc<DeviceStack>, level: usize) {
        let mut slot = self.stack.lock();
        assert!(slot.is_none(), "physical device object is supposed to be part of atmost 1 device stack!");
        *slot = Some((stack, level));
    }

    pub fn set_resources(&self, resource_list: ResList) {
        *self.resource_list.lock() = Some(resource_list);
    }
    
    pub fn get_resources(&self) -> Option<ResList> {
        *self.resource_list.lock()
    }

    // PDOs are created by a bus during enumerate and are always started — they
    // only carry bus resource info and never receive start/stop dispatches.
    // Returns false (and marks nothing) if the device was given a name --
    // PDOs must be unnamed, since no caller may issue IO requests to one.
    pub fn mark_started_pdo(&self) -> bool {
        if self.device.get_name().is_some() {
            return false;
        }
        self.is_pdo.store(true, Ordering::Release);
        self.set_state(DeviceState::Started);
        true
    }

    fn update_stack_state(&self, ls: LevelState) {
        if let Some((stack, level)) = self.stack.lock().clone() {
            stack.set_level_state(level, ls);
        }
    }

    pub fn read(&self, req: ReadRequest, is_interruptible: bool) -> Result<Status, KError> {
        Ok(io_request_sync(self, IrpMajor::Read, IrpMinor::None, req.buffer, req.offset, None, is_interruptible)?.status)
    }

    pub fn write(&self, req: ReadRequest, is_interruptible: bool) -> Result<Status, KError> {
        Ok(io_request_sync(self, IrpMajor::Write, IrpMinor::None, req.buffer, req.offset, None, is_interruptible)?.status)
    }

    pub fn attach_child(&self, child_id: usize) {
        self.children.lock().push(child_id);
        if let Some(child) = get_device(child_id) {
            child.set_parent(self.id);
        }
    }

    // Ordered start: the device below us in the stack (our parent) must already
    // be started, otherwise we cannot come up.
    fn do_start(&self, is_child: bool) -> Result<Status, KError> {
        let _g = self.config_guard();

        if self.is_class_device {
            assert!(!self.is_pdo());
            if self.state() == DeviceState::Removed {
                return Err(KError::DeviceRemoved);
            }
            self.set_state(DeviceState::Started);
            return Ok(Status::Success)
        }

        if let Some(pid) = self.parent_id() {
            if let Some(parent) = get_device(pid) {
                if !parent.is_started() {
                    info!("start: parent device {} of {} is not started", pid, self.id);
                    return Err(KError::DeviceStopped);
                }
            }
        }

        // PDO's are implicitly started
        let status = if !self.is_pdo() {
            let res_list = self.resource_list.lock().clone().map(|r| {ReqInfo{ res_list: r }});
            let irp = io_request_sync(self, IrpMajor::Pnp, IrpMinor::Start, EMPTY_REGION, 0, res_list, false)?;
            if irp.status == Status::Success {
                self.set_state(DeviceState::Started);
                self.update_stack_state(LevelState::Started);
                if !is_child {
                    self.enabled.store(true, Ordering::Release); 
                }
            }

            irp.status
        }
        else {
            Status::Success
        };
        
        // Now start all child devices that were previously enabled
        for cid in self.children_snapshot() {
            if let Some(child) = get_device(cid) {
                if child.is_pdo() || child.enabled() {
                    let _ = child.do_start(true);
                }
            }
        }

        Ok(status)
    }

    pub fn start(&self) -> Result<Status, KError> {
        self.do_start(false)
    }

    fn do_stop(&self, is_child: bool) -> Result<Status, KError> {
        let _g = self.config_guard();
        if self.is_class_device {
            if self.state() == DeviceState::Removed {
                return Err(KError::DeviceRemoved);
            }
            self.set_state(DeviceState::Stopped);
            return Ok(Status::Success);
        }

        // Stop all child devices first
        for cid in self.children_snapshot() {
            if let Some(child) = get_device(cid) {
                let _ = child.do_stop(true);
            }
        }

        Ok(self.stop_self(is_child))
    }

    fn quiesce_requests(&self, next_state: DeviceState) {
        let _rl = semaphore_guard(&self.request_lock);
        self.set_state(next_state);
    }

    fn wait_pending_irps_drained(&self) {
        loop {
            if self.pending_irps.lock().get_nodes() == 0 {
                break;
            }
            let _ = self.pending_irps_drained_event.wait(false);
        }
    }

    fn stop_self(&self, is_child: bool) -> Status {
        if self.is_pdo() {
            assert!(self.pending_irps.lock().get_nodes() == 0, "PDO {} has pending irps", self.id);
            return Status::Success;
        }
        if self.state() != DeviceState::Started {
            return Status::Success;
        }
        self.quiesce_requests(DeviceState::Stopping);
        let status = io_request_sync(self, IrpMajor::Pnp, IrpMinor::Stop, EMPTY_REGION, 0, None, false)
            .map(|irp| irp.status)
            .unwrap_or(Status::Failed);
        self.set_state(DeviceState::Stopped);
        self.update_stack_state(LevelState::Stopped);
        self.wait_pending_irps_drained();

        if !is_child {
            self.enabled.store(false, Ordering::Release);
        }
        status
    }

    // Every attached child is stopped first (we wait for each, regardless of its
    // result); then this device stops. PDOs skip their own stop dispatch but
    // still propagate the stop to their children.
    pub fn stop(&self) -> Result<Status, KError> {
        self.do_stop(false)
    }
}

impl Drop for DeviceObjectK {
    fn drop(&mut self) {
        crate::io_log!("Dropping device id: {}", self.id);
    }
}

pub struct ReadRequest {
    pub buffer: MemoryRegion,
    pub offset: usize
}

fn dispatch(driver: &DriverObjectK, major: IrpMajor, dev: *const DeviceObject, irp: *mut Irp) -> Status {
    let table = &unsafe { &*driver.driver.get() }.dispatch;
    match major {
        IrpMajor::Read    => table.invoke_read(dev, irp),
        IrpMajor::Write   => table.invoke_write(dev, irp),
        IrpMajor::Pnp     => table.invoke_pnp(dev, irp),
        IrpMajor::Control => table.invoke_control(dev, irp),
        IrpMajor::Open    => table.invoke_open(dev, irp),
        IrpMajor::Close   => table.invoke_close(dev, irp),
    }
}

fn allowed_in_state(state: DeviceState, major: IrpMajor, minor: IrpMinor, is_class: bool, is_pdo: bool) -> bool {
    if is_class {
        return match major {
            IrpMajor::Pnp => false,
            _ => state == DeviceState::Started,
        };
    }
    // No caller may send read/write/control/open/close to a PDO
    if is_pdo {
        match major {
            IrpMajor::Read | IrpMajor::Write | IrpMajor::Control | IrpMajor::Open | IrpMajor::Close => {
                return false;
            },
            _ => {}
        }
    }
    match (major, minor) {
        (IrpMajor::Read, _) | (IrpMajor::Write, _)   => state == DeviceState::Started,
        (IrpMajor::Control, _)
        | (IrpMajor::Open, _)
        | (IrpMajor::Close, _)                        => state == DeviceState::Started,
        (IrpMajor::Pnp, IrpMinor::Enumerate)
        | (IrpMajor::Pnp, IrpMinor::Query)
        | (IrpMajor::Pnp, IrpMinor::Resources)        => state == DeviceState::Started,
        (IrpMajor::Pnp, IrpMinor::Start)              => state == DeviceState::Stopped,
        (IrpMajor::Pnp, IrpMinor::Stop)               => state == DeviceState::Stopping,
        (IrpMajor::Pnp, IrpMinor::Remove)             => state == DeviceState::Removing,
        (IrpMajor::Pnp, IrpMinor::None)
        | (IrpMajor::Pnp, IrpMinor::RegisterKeyboardHandler)
        | (IrpMajor::Pnp, IrpMinor::SetForegroundPgrp)
        | (IrpMajor::Pnp, IrpMinor::SetControllingTty) => false,
    }
}

fn state_rejection_error(state: DeviceState, major: IrpMajor, minor: IrpMinor) -> KError {
    if state == DeviceState::Removed {
        return KError::DeviceRemoved;
    }
    if major == IrpMajor::Pnp && minor == IrpMinor::Start && state == DeviceState::Started {
        return KError::DeviceStarted;
    }
    KError::DeviceStopped
}

pub fn io_request_sync(
    dev: &DeviceObjectK,
    major: IrpMajor,
    minor: IrpMinor,
    buffer: MemoryRegion,
    offset: usize,
    req_info: Option<ReqInfo>,
    is_interruptible: bool
) -> Result<IrpResult, KError> {
    let event = KEvent::new(false);
    let mut result = IrpResult::default();

    // Held across the state check and IRP admission so a concurrent
    // stop/remove can't flip state in between
    let irp = {
        let _rl = semaphore_guard(&dev.request_lock);
        if !allowed_in_state(dev.state(), major, minor, dev.is_class_device, dev.is_pdo()) {
            return Err(state_rejection_error(dev.state(), major, minor));
        }
        if dev.driver.is_none() {
            return Err(KError::Unsupported);
        }
        let dev_handle = resolve_device(dev.device_ptr())
            .expect("device must still be registered while its request_lock is held");

        allocate_irp(
            major,
            minor,
            buffer,
            offset,
            dev_handle,
            Some(event.clone()),
            &mut result as *mut IrpResult,
            None,
            core::ptr::null_mut()
        )
    };

    if let Some(info) = req_info {
        unsafe { (*irp).req_info = info; }
    }

    let driver = dev.driver.as_ref().ok_or(KError::Unsupported)?;
    let status = dispatch(driver, major, dev.device_ptr(), irp);
    if status == Status::Pending {
        event.wait(is_interruptible);
    }
    else if status == Status::Unsupported {
        io_complete_irp(irp, status);
    }

    Ok(result)
}

pub fn io_request_async(
    dev: &DeviceObjectK,
    major: IrpMajor,
    minor: IrpMinor,
    buffer: MemoryRegion,
    offset: usize,
    req_info: Option<ReqInfo>,
    routine: extern "C" fn(*const IrpResult, *mut c_void),
    ctx: *mut c_void
) -> Result<Status, KError> {
    let irp = {
        let _rl = semaphore_guard(&dev.request_lock);
        if !allowed_in_state(dev.state(), major, minor, dev.is_class_device, dev.is_pdo()) {
            return Err(state_rejection_error(dev.state(), major, minor));
        }
        if dev.driver.is_none() {
            return Err(KError::Unsupported);
        }
        let dev_handle = resolve_device(dev.device_ptr())
            .expect("device must still be registered while its request_lock is held");

        allocate_irp(
            major,
            minor,
            buffer,
            offset,
            dev_handle,
            None,
            core::ptr::null_mut(),
            Some(routine),
            ctx
        )
    };

    if let Some(info) = req_info {
        unsafe { (*irp).req_info = info; }
    }

    let driver = dev.driver.as_ref().ok_or(KError::Unsupported)?;
    let status = dispatch(driver, major, dev.device_ptr(), irp);
    if status == Status::Unsupported {
        io_complete_irp(irp, status);
    }
    Ok(status)
}

pub fn driver_invoke_add(driver: &DriverHandle, pdo: *const DeviceObject) -> Status {
    unsafe { &*driver.driver.get() }.dispatch.invoke_add(driver.driver.get(), pdo)
}

pub fn get_device(id: usize) -> Option<DeviceHandleK> {
    DEVICE_REGISTRY.lock().get(&id).cloned()
}


pub fn root_device() -> DeviceHandleK {
    ROOT_DEVICE.get().expect("root device not initialized").clone()
}

fn create_root_device() -> DeviceHandleK {
    let id = NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed);
    let dev = Arc::new_in(
        DeviceObjectK {
            id,
            device: DeviceObject::new(id, None, core::ptr::null_mut()),
            driver: None,
            state: AtomicUsize::new(DeviceState::Started as usize),
            is_pdo: AtomicBool::new(true),
            is_class_device: false,
            parent: Spinlock::new(None),
            enabled: AtomicBool::new(false),
            children: Spinlock::new(Vec::new()),
            stack: Spinlock::new(None),
            config_sem: KSem::new(1, 1),
            pending_irps: Spinlock::new(List::new()),
            resource_list: Spinlock::new(None),
            request_lock: KSem::new(1, 1),
            pending_irps_drained_event: KEvent::new(true)
        },
        PoolAllocatorGlobal
    );
    DEVICE_REGISTRY.lock().insert(id, dev.clone());
    dev
}

fn load_driver(path: &str) -> Result<DriverHandle, KError> {
    let image = load_image(path, false)?;
    let id = NEXT_DRIVER_ID.fetch_add(1, Ordering::Relaxed);

    let (name, entry_addr) = {
        let guard = image.lock();
        (guard.kernel().name, guard.kernel().driver_init_address)
    };

    let driver_k = DriverObjectK {
        image: UnsafeCell::new(Some(image)),
        state: AtomicUsize::new(DriverState::Loaded as usize),
        driver: UnsafeCell::new(DriverObject::new(id, StrRef::from_str(name))),
        driver_guard: KSem::new(1,1),
        devices: Spinlock::new(Vec::new())
    };

    if entry_addr.is_none() {
        return Err(KError::ModuleNotDriver);
    }

    let entry: extern "C" fn(*mut DriverObject) -> Status = unsafe { core::mem::transmute(entry_addr.unwrap()) };

    // Add to registry first so that driver init can create devices too 
    let driver = Arc::new_in(driver_k, PoolAllocatorGlobal);
    DRIVER_REGISTRY.lock().insert(id, driver.clone());
    if entry(driver.driver.get()) == Status::Failed {
        info!("Driver init for {} failed!", path);
        DRIVER_REGISTRY.lock().remove(&id);
        return Err(KError::DriverLoadFailed);
    }

    Ok(driver)
}

pub fn load_driver_by_name(name: &str) -> Result<DriverHandle, KError> {
    let _guard = semaphore_guard(
        DRIVER_LOAD_LOCK.get().expect("io::init() not called before load_driver_by_name()")
    );

    if let Some(driver) = DRIVER_BY_NAME.lock().get(name) {
        return Ok(driver.clone());
    }

    let path = stack::get_driver_path(name).ok_or(KError::InvalidArgument)?;
    let driver = load_driver(&path)?;
    DRIVER_BY_NAME.lock().insert(name.to_string(), driver.clone());
    Ok(driver)
}

pub fn unload_driver(name: &str) -> Result<(), KError> {
    let _guard = semaphore_guard(
        DRIVER_LOAD_LOCK.get().expect("io::init() not called before load_driver_by_name()")
    );
    
    let driver = if let Some(driver) = DRIVER_BY_NAME.lock().get(name) {
        driver.clone()
    }
    else {
        return Err(KError::InvalidArgument);
    };

    // Remove all devices within the driver
    {
        let _guard = semaphore_guard(&driver.driver_guard);
        let device_list = driver.devices.lock().clone();   
        for id in device_list {
            if let Some(device) = get_device(id) {
                remove_device(&device);
            }
        }

        DRIVER_REGISTRY.lock().remove(unsafe {&(*driver.driver.get()).id});
        DRIVER_BY_NAME.lock().remove(unsafe {(*driver.driver.get()).get_name()});

        // Call driver unload function (if it exists)
        // A driver is required to have init function, but unload is optional
        let image = unsafe { &mut *driver.image.get() }.take()
            .expect("No driver image found in driver object!");
        let unload_addr_opt = image.lock().kernel().driver_unload_address;  
        if let Some(unload_fn_addr) = unload_addr_opt {
            let unload_fn: extern "C" fn(*mut kernel_intf::driver::DriverObject) = unsafe {
                core::mem::transmute(unload_fn_addr)
            };

            unload_fn(driver.driver.get());
        }

        drop(image);
        driver.set_state(DriverState::Unloaded);
    }

    Ok(())
}

// Returns null on failure. Failure causes: unknown driver id, or the supplied
// name is already registered for another device 
#[unsafe(no_mangle)]
pub extern "C" fn io_create_device(
    driver_id: usize,
    name: StrRef,
    ctx: *mut c_void,
    parent: *const DeviceObject,
    is_class: bool
) -> *mut DeviceObject {
    let driver = match DRIVER_REGISTRY.lock().get(&driver_id) {
        Some(driver) => driver.clone(),
        None => {
            info!("io_create_device: unknown driver id {}", driver_id);
            return core::ptr::null_mut();
        }
    };
    
    let _guard = semaphore_guard(&driver.driver_guard);
    if driver.state() == DriverState::Unloaded {
        info!("Device could not be created since driver is unloading!");
        return core::ptr::null_mut();
    } 

    let id = NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed);
    // Class devices have no parent; normal devices may be unattached if parent is null.
    let parent_id = if is_class || parent.is_null() { None } else { Some(unsafe { (*parent).id }) };
    let name_ref = unsafe { name.as_str() };
    let name = if name_ref.is_empty() { None } else { Some(name_ref) };

    // Reject duplicate names
    if let Some(n) = name {
        if DEVICE_BY_NAME.lock().contains_key(n) {
            info!("io_create_device: duplicate device name '{}'", n);
            return core::ptr::null_mut();
        }
    }

    let device = Arc::new_in(
        DeviceObjectK {
            id,
            device: DeviceObject::new(id, name, ctx),
            driver: Some(driver.clone()),
            state: AtomicUsize::new(DeviceState::Stopped as usize),
            is_pdo: AtomicBool::new(false),
            is_class_device: is_class,
            enabled: AtomicBool::new(false),
            parent: Spinlock::new(parent_id),
            children: Spinlock::new(Vec::new()),
            stack: Spinlock::new(None),
            config_sem: KSem::new(1, 1),
            pending_irps: Spinlock::new(List::new()),
            resource_list: Spinlock::new(None),
            request_lock: KSem::new(1, 1),
            pending_irps_drained_event: KEvent::new(true)
        },
        PoolAllocatorGlobal
    );

    // Add the device to the driver
    driver.devices.lock().push(id);

    let device_ptr = device.device_ptr() as *mut DeviceObject;
    DEVICE_REGISTRY.lock().insert(id, device.clone());
    if let Some(n) = name {
        DEVICE_BY_NAME.lock().insert(n.to_string(), device);
    }

    // Attach the new device under its parent (if any) so io can discover it.
    if let Some(pid) = parent_id {
        if let Some(parent_dev) = get_device(pid) {
            parent_dev.children.lock().push(id);
        }
    }

    crate::io_log!("io_create_device: device {} (driver {}) parent {:?} class={}", id, driver_id, parent_id, is_class);
    device_ptr
}

pub fn resolve_device(ptr: *const DeviceObject) -> Option<DeviceHandleK> {
    if ptr.is_null() {
        return None;
    }
    get_device(unsafe { (*ptr).id })
}


pub fn open_device_handle(name: &str) -> Result<OpenDeviceHandle, KError> {
    let dev = DEVICE_BY_NAME.lock().get(name).cloned().ok_or(KError::InvalidArgument)?;
    match dev.state() {
        DeviceState::Stopping | DeviceState::Stopped => {
            return Err(KError::DeviceStopped);
        },
        _ => {}
    }
    let _ = io_request_sync(&dev, IrpMajor::Open, IrpMinor::None, EMPTY_REGION, 0, None, false);
    Ok(Arc::new_in(OpenDeviceHandleInner { dev }, PoolAllocatorGlobal))
}

// Cancel every non-PNP IRP issued by the current thread to this device.
pub fn cancel_pending_irp(dev: &DeviceObjectK) {
    match dev.state() {
        DeviceState::Removed | DeviceState::Removing => {
            return;
        },
        _ => {}
    }
    let tid = sched::get_current_task_id().expect("cancel_pending_irp called from idle task!");
    let snapshot: Vec<IrpPtr> = dev.pending_irps
    .lock()
    .iter()
    .filter(|&p| unsafe { (***p).thread_id == tid && (***p).major_code != IrpMajor::Pnp })
    .map(|p| **p)
    .collect();

    for irp in snapshot {
        cancel_irp(irp, dev);
    }
}

// Tear down a device subtree. 
pub fn remove_device(dev: &DeviceObjectK) {
    let _g = dev.config_guard();

    if dev.is_class_device {
        if dev.state() != DeviceState::Removed {
            // Class devices skip Pnp dispatch entirely, so there's no
            // separate "stop" step to give the driver a chance to fail
            // in-flight requests -- just close admission and wait them out.
            dev.quiesce_requests(DeviceState::Removed);
            dev.wait_pending_irps_drained();
            DEVICE_REGISTRY.lock().remove(&dev.id);
            if let Some(n) = dev.device.get_name() {
                DEVICE_BY_NAME.lock().remove(n);
            }
        }

        return;
    }

    // We get the name all the way here since we can't guarantee that the name 
    // string exists within the driver once remove request is sent.
    // The driver is expected to deallocate all device related resources (including buffer storing its name)
    let dev_name = dev.device.get_name().map(String::from);

    for cid in dev.children_snapshot() {
        if let Some(child) = get_device(cid) {
            remove_device(&child);
        }
    }

    dev.stop_self(false);

    let cur = dev.state();
    if cur != DeviceState::Removing && cur != DeviceState::Removed {
        if dev.is_pdo() {
            assert!(dev.pending_irps.lock().get_nodes() == 0, "PDO {} has pending irps", dev.id);
        }
        dev.set_state(DeviceState::Removing);
        let _ = io_request_sync(dev, IrpMajor::Pnp, IrpMinor::Remove, EMPTY_REGION, 0, None, false);
        dev.set_state(DeviceState::Removed);
    } else {
        return;
    }

    crate::io_log!("Removing device id {}", dev.id);

    if let Some(pid) = dev.parent_id() {
        if let Some(parent) = get_device(pid) {
            parent.children.lock().retain(|&c| c != dev.id);
        }
    }

    deallocate_device_resources(dev);

    // Remove device from driver
    let driver = dev.driver.as_ref().expect("No driver found for device object!");
    driver.devices.lock().retain(|&c| c != dev.id);

    // Notify the device's stack instance
    if let Some((stack, level)) = dev.stack.lock().take() {
        stack::on_device_removed(&stack, level);
    }

    DEVICE_REGISTRY.lock().remove(&dev.id);
    if let Some(n) = &dev_name {
        kernel_intf::debug!("Removing device {} from registry", n);
        DEVICE_BY_NAME.lock().remove(n);
    }
    crate::io_log!("remove_device: dropped device {}", dev.id);
}

#[unsafe(no_mangle)]
pub extern "C" fn io_get_driver_id(device: *const DeviceObject) -> usize {
    resolve_device(device)
        .and_then(|dev| dev.driver.as_ref().map(|d| unsafe {&*d.driver.get()}.id))
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn io_invalidate_device(device: *const DeviceObject) -> Status {
    match resolve_device(device) {
        Some(dev) => {
            super::pnp::pnp_post(super::pnp::PnpRequest::InvalidateDevice { device_id: dev.id() });
            Status::Success
        }
        None => Status::Failed
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn io_start_device_ffi(device: *const DeviceObject) -> Status {
    let dev = match resolve_device(device) {
        Some(d) => d,
        None => return Status::Failed,
    };
    let res = dev.start();
    if res.is_err() {
        Status::Failed
    }
    else {
        Status::Success
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn io_stop_device_ffi(device: *const DeviceObject) -> Status {
    let dev = match resolve_device(device) {
        Some(d) => d,
        None => return Status::Failed,
    };
    let res = dev.stop();
    if res.is_err() {
        Status::Failed
    }
    else {
        Status::Success
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn io_remove_device_ffi(device: *const DeviceObject) -> Status {
    let dev = match resolve_device(device) {
        Some(d) => d,
        None => return Status::Failed,
    };
    
    remove_device(&dev);
    Status::Success
}

#[unsafe(no_mangle)]
pub extern "C" fn io_send_request_ffi(
    device: *const DeviceObject,
    major: usize,
    minor: usize,
    buf_base: usize,
    buf_size: usize,
    offset: usize,
    req_info_ptr: *const ReqInfo,
    completion: Option<extern "C" fn(*const IrpResult, *mut c_void)>,
    completion_ctx: *mut c_void
) -> Status {
    let dev = match resolve_device(device) {
        Some(dev) => dev,
        None => return Status::Failed
    };
    let major = match IrpMajor::from_usize(major) {
        Some(major) => major,
        None => return Status::Failed
    };
    let minor = IrpMinor::from_usize(minor).unwrap_or(IrpMinor::None);
    let buffer = MemoryRegion { base_address: buf_base, size: buf_size };
    let req_info = if req_info_ptr.is_null() {
        None
    } else {
        Some(unsafe { *req_info_ptr })
    };

    let result = match completion {
        None    => io_request_sync(&dev, major, minor, buffer, offset, req_info, false).map(|r| r.status),
        Some(r) => io_request_async(&dev, major, minor, buffer, offset, req_info, r, completion_ctx)
    };

    result.unwrap_or(Status::Failed)
}

#[unsafe(no_mangle)]
extern "C" fn io_complete_irp_ffi(irp: *mut Irp, status: Status) {
    assert!(status == Status::Success || status == Status::Failed || status == Status::Cancelled || status == Status::Unsupported);
    unsafe { (*irp).status = status }
    let completion_routine = unsafe { (*irp).completion_routine };
    let completion_ctx = unsafe { (*irp).completion_ctx };
    (completion_routine)(irp, completion_ctx);
}

#[unsafe(no_mangle)]
extern "C" fn io_start_processing_ffi(irp: *mut Irp) -> bool {
    disable_preemption();
    let irp = unsafe { &mut *irp };
    acquire_spinlock(&mut irp.cancel_lock);
    if irp.is_cancelled {
        crate::io_log!("Cancelled arm in io_start_processing by thread: {} on irp {:#X}", irp.thread_id, irp as *const Irp as usize);
        let ctx = irp.completion_ctx as *mut AsyncCtx;
        release_spinlock(&mut irp.cancel_lock);
        deallocate_irp(irp, ctx);
        enable_preemption();

        false
    }
    else {
        irp.cancel_routine = None;
        release_spinlock(&mut irp.cancel_lock);
        enable_preemption();
        true
    }
}

#[unsafe(no_mangle)]
extern "C" fn io_set_cancel_routine_ffi(
    irp: *mut Irp, 
    routine: extern "C" fn(*const DeviceObject, *mut Irp)
) {
    let irp = unsafe { &mut *irp };
    acquire_spinlock(&mut irp.cancel_lock);
    crate::io_log!("Setting cancel routine by thread: {} on irp {:#X}", irp.thread_id, irp as *const Irp as usize);
    
    assert!(!irp.is_cancelled);
    assert!(irp.cancel_routine.is_none());
    irp.cancel_routine = Some(routine);
    release_spinlock(&mut irp.cancel_lock);
}

pub fn deallocate_irp(irp: *mut Irp, ctx: *mut AsyncCtx) {
    crate::io_log!("Deallocating irp {:#X} by thread: {}", irp.addr(), unsafe{(*irp).thread_id});
    disable_preemption();
    drop(unsafe { Box::from_raw_in(irp, PoolAllocatorGlobal) });
    drop(unsafe { Box::from_raw_in(ctx, PoolAllocatorGlobal) });
    enable_preemption();
}

fn open_device_handler(name: &str, _flags: u64) -> Result<crate::sched::Handle, kernel_intf::KError> {
    open_device_handle(name).map(crate::sched::Handle::DeviceHandle)
}

pub fn init() {
    DRIVER_LOAD_LOCK.call_once(|| KSem::new(1, 1));
    ROOT_DEVICE.call_once(create_root_device);
    crate::object::register_object_type("device", open_device_handler)
        .expect("device object type already registered");

    // Parse boot.conf and bring up every Root stack. Enumeration inside the
    // root bus drivers recursively detects and loads child stacks.
    stack::load_boot_config();
    stack::load_root_stacks(None);

    super::pnp::start_worker();
}
