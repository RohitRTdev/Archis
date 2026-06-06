use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use common::{MemoryRegion, StrRef};
use kernel_intf::driver::{
    DeviceObject, DriverObject, EMPTY_REGION, Irp, IrpMajor, IrpMinor, IrpResult, Status
};
use kernel_intf::{acquire_spinlock, release_spinlock};
use kernel_intf::list::{DynList, List};
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::{KError, info};

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

#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DeviceState {
    Stopped  = 0,
    Started  = 1,
    Stopping = 2,
    Removing = 3,
    Removed  = 4
}

pub struct DriverObjectK {
    _image: LoadedImage,
    driver: DriverObject
}

pub struct DeviceObjectK {
    id: usize,
    device: DeviceObject,
    // This is only None for root device
    driver: Option<DriverHandle>,
    state: AtomicUsize,
    is_pdo: AtomicBool,
    parent: Spinlock<Option<usize>>,
    children: Spinlock<Vec<usize>>,
    stack: Spinlock<Option<(Arc<DeviceStack>, usize)>>,
    // Serializes start/stop/query/enumerate/remove/add for this device.
    // Read/write mutual exclusion is left to driver writer.
    config_sem: KSem,
    pub pending_irps: Spinlock<DynList<IrpPtr>>
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

    pub fn is_pdo(&self) -> bool {
        self.is_pdo.load(Ordering::Acquire)
    }

    pub fn parent_id(&self) -> Option<usize> {
        *self.parent.lock()
    }

    pub fn set_parent(&self, pid: usize) {
        *self.parent.lock() = Some(pid);
    }

    fn state(&self) -> DeviceState {
        unsafe { core::mem::transmute(self.state.load(Ordering::Acquire)) }
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

    // PDOs are created by a bus during enumerate and are always started — they
    // only carry bus resource info and never receive start/stop dispatches.
    pub fn mark_started_pdo(&self) {
        self.is_pdo.store(true, Ordering::Release);
        self.set_state(DeviceState::Started);
    }

    fn update_stack_state(&self, ls: LevelState) {
        if let Some((stack, level)) = self.stack.lock().clone() {
            stack.set_level_state(level, ls);
        }
    }

    pub fn read(&self, req: ReadRequest) -> Result<Status, KError> {
        Ok(io_request_sync(self, IrpMajor::Read, IrpMinor::None, req.buffer, req.offset)?.status)
    }

    pub fn write(&self, req: ReadRequest) -> Result<Status, KError> {
        Ok(io_request_sync(self, IrpMajor::Write, IrpMinor::None, req.buffer, req.offset)?.status)
    }

    pub fn attach_child(&self, child_id: usize) {
        self.children.lock().push(child_id);
        if let Some(child) = get_device(child_id) {
            child.set_parent(self.id);
        }
    }

    // Ordered start: the device below us in the stack (our parent) must already
    // be started, otherwise we cannot come up.
    pub fn start(&self) -> Result<Status, KError> {
        let _g = self.config_guard();

        if let Some(pid) = self.parent_id() {
            if let Some(parent) = get_device(pid) {
                if !parent.is_started() {
                    info!("start: parent device {} of {} is not started", pid, self.id);
                    return Err(KError::DeviceStopped);
                }
            }
        }

        let irp = io_request_sync(self, IrpMajor::Pnp, IrpMinor::Start, EMPTY_REGION, 0)?;
        if irp.status == Status::Success {
            self.set_state(DeviceState::Started);
            self.update_stack_state(LevelState::Started);
        }
        Ok(irp.status)
    }

    fn stop_self(&self) -> Status {
        if self.is_pdo() || self.state() != DeviceState::Started {
            return Status::Success;
        }
        self.set_state(DeviceState::Stopping);
        let status = io_request_sync(self, IrpMajor::Pnp, IrpMinor::Stop, EMPTY_REGION, 0)
            .map(|irp| irp.status)
            .unwrap_or(Status::Failed);
        self.set_state(DeviceState::Stopped);
        self.update_stack_state(LevelState::Stopped);
        status
    }

    // Every attached child is stopped first (we wait for each, regardless of its
    // result); then this device stops. PDOs skip their own stop dispatch but
    // still propagate the stop to their children.
    pub fn stop(&self) -> Result<Status, KError> {
        let _g = self.config_guard();

        for cid in self.children_snapshot() {
            if let Some(child) = get_device(cid) {
                let _ = child.stop();
            }
        }

        Ok(self.stop_self())
    }
}

impl Drop for DeviceObjectK {
    fn drop(&mut self) {
        info!("Dropping device id: {}", self.id);
    }
}

pub struct ReadRequest {
    pub buffer: MemoryRegion,
    pub offset: usize
}

fn dispatch(driver: &DriverObjectK, major: IrpMajor, dev: *const DeviceObject, irp: *mut Irp) -> Status {
    let table = &driver.driver.dispatch;
    match major {
        IrpMajor::Read => table.invoke_read(dev, irp),
        IrpMajor::Write => table.invoke_write(dev, irp),
        IrpMajor::Pnp => table.invoke_pnp(dev, irp)
    }
}

fn allowed_in_state(state: DeviceState, major: IrpMajor, minor: IrpMinor) -> bool {
    match (major, minor) {
        (IrpMajor::Read, _) | (IrpMajor::Write, _)  => state == DeviceState::Started,
        (IrpMajor::Pnp,  IrpMinor::Enumerate)
        | (IrpMajor::Pnp, IrpMinor::Query)          => state == DeviceState::Started,
        (IrpMajor::Pnp,  IrpMinor::Start)           => state == DeviceState::Stopped,
        (IrpMajor::Pnp,  IrpMinor::Stop)            => state == DeviceState::Stopping,
        (IrpMajor::Pnp,  IrpMinor::Remove)          => state == DeviceState::Removing,
        (IrpMajor::Pnp,  IrpMinor::None)            => false,
    }
}

pub fn io_request_sync(
    dev: &DeviceObjectK,
    major: IrpMajor,
    minor: IrpMinor,
    buffer: MemoryRegion,
    offset: usize
) -> Result<IrpResult, KError> {
    if !allowed_in_state(dev.state(), major, minor) {
        return Err(KError::DeviceStopped);
    }
    let driver = dev.driver.as_ref().ok_or(KError::Unsupported)?;

    let event = KEvent::new(false);
    let mut result = IrpResult::default();

    let irp = allocate_irp(
        major,
        minor,
        buffer,
        offset,
        dev.device_ptr(),
        Some(event.clone()),
        &mut result as *mut IrpResult,
        None,
        core::ptr::null_mut()
    );

    let status = dispatch(driver, major, dev.device_ptr(), irp);
    if status == Status::Pending {
        event.wait().expect("io_request_sync: completion wait failed with a pending IRP outstanding");
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
    routine: extern "C" fn(*const IrpResult, *mut c_void),
    ctx: *mut c_void
) -> Result<Status, KError> {
    if !allowed_in_state(dev.state(), major, minor) {
        return Err(KError::DeviceStopped);
    }
    let driver = dev.driver.as_ref().ok_or(KError::Unsupported)?;

    let irp = allocate_irp(
        major,
        minor,
        buffer,
        offset,
        dev.device_ptr(),
        None,
        core::ptr::null_mut(),
        Some(routine),
        ctx
    );

    let status = dispatch(driver, major, dev.device_ptr(), irp);
    if status == Status::Unsupported {
        io_complete_irp(irp, status);
    }
    Ok(status)
}

pub fn driver_invoke_add(driver: &DriverHandle, pdo: *const DeviceObject) -> Status {
    driver.driver.dispatch.invoke_add(&driver.driver, pdo)
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
            parent: Spinlock::new(None),
            children: Spinlock::new(Vec::new()),
            stack: Spinlock::new(None),
            config_sem: KSem::new(1, 1),
            pending_irps: Spinlock::new(List::new())
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
        (guard.name, guard.info.entry)
    };

    let mut driver_k = DriverObjectK {
        _image: image,
        driver: DriverObject::new(id, StrRef::from_str(name))
    };

    let entry: extern "C" fn(*mut DriverObject) -> Status = unsafe { core::mem::transmute(entry_addr) };
    if entry(&mut driver_k.driver) == Status::Failed {
        info!("Driver init for {} failed!", path);
        return Err(KError::DriverLoadFailed);
    }

    let driver = Arc::new_in(driver_k, PoolAllocatorGlobal);
    DRIVER_REGISTRY.lock().insert(id, driver.clone());
    Ok(driver)
}

pub fn load_driver_by_name(name: &str) -> Result<DriverHandle, KError> {
    let _guard = semaphore_guard(
        DRIVER_LOAD_LOCK.get().expect("io::init() not called before load_driver_by_name()")
    );

    if let Some(driver) = DRIVER_BY_NAME.lock().get(name) {
        return Ok(driver.clone());
    }
    let path = format!("/sys/drivers/lib{}.so", name);
    let driver = load_driver(&path)?;
    DRIVER_BY_NAME.lock().insert(name.to_string(), driver.clone());
    Ok(driver)
}

// Returns null on failure. Failure causes: unknown driver id, or the supplied
// name is already registered for another device 
#[unsafe(no_mangle)]
pub extern "C" fn io_create_device(
    driver_id: usize,
    name: StrRef,
    ctx: *mut c_void,
    parent: *const DeviceObject
) -> *mut DeviceObject {
    let driver = match DRIVER_REGISTRY.lock().get(&driver_id) {
        Some(driver) => driver.clone(),
        None => {
            info!("io_create_device: unknown driver id {}", driver_id);
            return core::ptr::null_mut();
        }
    };

    let id = NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed);
    // A null parent leaves the device unattached (enumerate PDOs attach later).
    let parent_id = if parent.is_null() { None } else { Some(unsafe { (*parent).id }) };
    let name_ref = unsafe { name.as_str() };
    let name = if name_ref.is_empty() {
        None
    }
    else {
        Some(name_ref)
    };

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
            driver: Some(driver),
            state: AtomicUsize::new(DeviceState::Stopped as usize),
            is_pdo: AtomicBool::new(false),
            parent: Spinlock::new(parent_id),
            children: Spinlock::new(Vec::new()),
            stack: Spinlock::new(None),
            config_sem: KSem::new(1, 1),
            pending_irps: Spinlock::new(List::new())
        },
        PoolAllocatorGlobal
    );

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

    info!("io_create_device: device {} (driver {}) parent {:?}", id, driver_id, parent_id);
    device_ptr
}

pub fn resolve_device(ptr: *const DeviceObject) -> Option<DeviceHandleK> {
    if ptr.is_null() {
        return None;
    }
    get_device(unsafe { (*ptr).id })
}

pub fn open_device_handle(name: &str) -> Result<DeviceHandleK, KError> {
    DEVICE_BY_NAME.lock().get(name).cloned().ok_or(KError::InvalidArgument)
}

// Cancel every non-PNP IRP issued by the current thread to this device.
pub fn cancel_pending_irp(dev: &DeviceHandleK) {
    let tid = sched::get_current_task_id().expect("cancel_pending_irp called from idle task!");
    let snapshot: Vec<IrpPtr> = dev.pending_irps
    .lock()
    .iter()
    .filter(|&p| unsafe { (***p).thread_id == tid && (***p).major_code != IrpMajor::Pnp })
    .map(|p| **p)
    .collect();

    for irp in snapshot {
        cancel_irp(irp);
    } 
}

// Tear down a device subtree. 
pub fn remove_device(dev: &DeviceObjectK) {
    let _g = dev.config_guard();

    for cid in dev.children_snapshot() {
        if let Some(child) = get_device(cid) {
            remove_device(&child);
        }
    }

    dev.stop_self();

    let cur = dev.state();
    if cur != DeviceState::Removing && cur != DeviceState::Removed {
        dev.set_state(DeviceState::Removing);
        let _ = io_request_sync(dev, IrpMajor::Pnp, IrpMinor::Remove, EMPTY_REGION, 0);
        dev.set_state(DeviceState::Removed);
    } else {
        return;
    }

    if let Some(pid) = dev.parent_id() {
        if let Some(parent) = get_device(pid) {
            parent.children.lock().retain(|&c| c != dev.id);
        }
    }

    // Notify the device's stack instance
    if let Some((stack, level)) = dev.stack.lock().take() {
        stack::on_device_removed(&stack, level);
    }

    DEVICE_REGISTRY.lock().remove(&dev.id);
    if let Some(n) = dev.device.get_name() {
        DEVICE_BY_NAME.lock().remove(n);
    }
    info!("remove_device: dropped device {}", dev.id);
}

#[unsafe(no_mangle)]
pub extern "C" fn io_get_driver_id(device: *const DeviceObject) -> usize {
    resolve_device(device)
        .and_then(|dev| dev.driver.as_ref().map(|d| d.driver.id))
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
pub extern "C" fn io_send_request(
    device: *const DeviceObject,
    major: usize,
    minor: usize,
    buf_base: usize,
    buf_size: usize,
    offset: usize,
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

    let result = match completion {
        None => io_request_sync(&dev, major, minor, buffer, offset).map(|r| r.status),
        Some(routine) => io_request_async(&dev, major, minor, buffer, offset, routine, completion_ctx)
    };

    result.unwrap_or(Status::Failed)
}

#[unsafe(no_mangle)]
extern "C" fn io_complete_irp(irp: *mut Irp, status: Status) {
    assert!(status == Status::Success || status == Status::Failed || status == Status::Cancelled || status == Status::Unsupported);
    unsafe { (*irp).status = status }
    let completion_routine = unsafe { (*irp).completion_routine };
    let completion_ctx = unsafe { (*irp).completion_ctx };
    (completion_routine)(irp, completion_ctx);
}

#[unsafe(no_mangle)]
extern "C" fn io_start_processing(irp: *mut Irp) -> bool {
    disable_preemption();
    let irp = unsafe { &mut *irp };
    unsafe { acquire_spinlock(&mut irp.cancel_lock); }
    if irp.is_cancelled {
        info!("Cancelled arm in io_start_processing by thread: {} on irp {:#X}", irp.thread_id, irp as *const Irp as usize);
        let ctx = irp.completion_ctx as *mut AsyncCtx;
        unsafe { release_spinlock(&mut irp.cancel_lock); }
        deallocate_irp(irp, ctx);
        enable_preemption();

        false
    }
    else {
        irp.cancel_routine = None;
        unsafe { release_spinlock(&mut irp.cancel_lock); }
        enable_preemption();
        true
    }
}

#[unsafe(no_mangle)]
extern "C" fn io_set_cancel_routine(
    irp: *mut Irp, 
    routine: extern "C" fn(*const DeviceObject, *mut Irp)
) {
    let irp = unsafe { &mut *irp };
    unsafe { acquire_spinlock(&mut irp.cancel_lock); }
    info!("Setting cancel routine by thread: {} on irp {:#X}", irp.thread_id, irp as *const Irp as usize);
    
    assert!(!irp.is_cancelled);
    assert!(irp.cancel_routine.is_none());
    irp.cancel_routine = Some(routine);
    unsafe { release_spinlock(&mut irp.cancel_lock); }
}

pub fn deallocate_irp(irp: *mut Irp, ctx: *mut AsyncCtx) {
    info!("Deallocating irp {:#X} by thread: {}", irp.addr(), unsafe{(*irp).thread_id});
    disable_preemption();
    drop(unsafe { Box::from_raw_in(irp, PoolAllocatorGlobal) });
    drop(unsafe { Box::from_raw_in(ctx, PoolAllocatorGlobal) });
    enable_preemption();
}

// Exported to drivers: spawn a kernel worker thread. The handler must never
// return; it should finish by calling exit_kernel_thread. Stopgap until a
// proper DPC/bottom-half mechanism exists.
#[unsafe(no_mangle)]
#[allow(improper_ctypes_definitions)]
pub extern "C" fn create_kernel_thread(handler: fn() -> !) -> KError {
    match crate::sched::create_thread(handler) {
        Ok(_) => KError::Success,
        Err(e) => e
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn exit_kernel_thread() -> ! {
    crate::sched::exit_thread()
}

pub fn init() {
    DRIVER_LOAD_LOCK.call_once(|| KSem::new(1, 1));
    ROOT_DEVICE.call_once(create_root_device);

    // Parse boot.conf and bring up every Root stack. Enumeration inside the
    // root bus drivers recursively detects and loads child stacks.
    stack::load_boot_config();
    stack::load_root_stacks();

    super::pnp::start_worker();
}
