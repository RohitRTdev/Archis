use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use common::{MemoryRegion, StrRef};
use kernel_intf::driver::{DeviceObject, DriverObject, Irp, IrpMajor, IrpMinor, Status};
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::{KError, info};

use crate::loader::{LoadedImage, load_image};
use crate::sync::{KEvent, KSem, Once, Spinlock};

use super::stack::{self, DeviceStack, LevelState};

pub type DriverHandle = Arc<DriverObjectK, PoolAllocatorGlobal>;
pub type DeviceHandleK = Arc<DeviceObjectK, PoolAllocatorGlobal>;

const EMPTY_REGION: MemoryRegion = MemoryRegion { base_address: 0, size: 0 };

static NEXT_DRIVER_ID: AtomicUsize = AtomicUsize::new(0);
static NEXT_DEVICE_ID: AtomicUsize = AtomicUsize::new(0);
static DRIVER_REGISTRY: Spinlock<BTreeMap<usize, DriverHandle>> = Spinlock::new(BTreeMap::new());
static DRIVER_BY_NAME: Spinlock<BTreeMap<String, DriverHandle>> = Spinlock::new(BTreeMap::new());
static DEVICE_REGISTRY: Spinlock<BTreeMap<usize, DeviceHandleK>> = Spinlock::new(BTreeMap::new());
static ROOT_DEVICE: Once<DeviceHandleK> = Once::new();

#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum DeviceState {
    Stopped = 0,
    Started = 1
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
    config_sem: KSem
}

pub struct ConfigGuard<'a> {
    sem: &'a KSem
}

impl Drop for ConfigGuard<'_> {
    fn drop(&mut self) {
        self.sem.signal();
    }
}

impl DeviceObjectK {
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

    pub fn config_guard(&self) -> ConfigGuard<'_> {
        self.config_sem.wait().expect("config sem wait failed");
        ConfigGuard { sem: &self.config_sem }
    }

    fn is_started(&self) -> bool {
        self.state.load(Ordering::Acquire) == DeviceState::Started as usize
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
        *self.stack.lock() = Some((stack, level));
    }

    // PDOs are created by a bus during enumerate and are always "started" — they
    // only carry bus resource info and never receive start/stop dispatches.
    pub fn mark_started_pdo(&self) {
        self.is_pdo.store(true, Ordering::Release);
        self.state.store(DeviceState::Started as usize, Ordering::Release);
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

        let irp = io_request_sync(self, IrpMajor::Start, IrpMinor::None, EMPTY_REGION, 0)?;
        if irp.status == Status::Success {
            self.state.store(DeviceState::Started as usize, Ordering::Release);
            self.update_stack_state(LevelState::Started);
        }
        Ok(irp.status)
    }

    // Stop this single device (caller must hold the config guard). Flips the
    // state to Stopped before dispatching, so concurrent read/write get
    // DeviceStopped and the driver's stop handler sees no new requests.
    fn stop_self(&self) -> Status {
        if self.is_pdo() || !self.is_started() {
            return Status::Success;
        }
        self.state.store(DeviceState::Stopped as usize, Ordering::Release);
        let status = io_request_sync(self, IrpMajor::Stop, IrpMinor::None, EMPTY_REGION, 0)
            .map(|irp| irp.status)
            .unwrap_or(Status::Failed);
        self.update_stack_state(LevelState::Stopped);
        status
    }

    // Every attached child is stopped first (we wait for each, regardless of its
    // result); then this device stops. PDOs skip their own stop dispatch but
    // still propagate the stop to their children.
    pub fn stop(&self) -> Result<Status, KError> {
        for cid in self.children_snapshot() {
            if let Some(child) = get_device(cid) {
                let _ = child.stop();
            }
        }

        let _g = self.config_guard();
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
        IrpMajor::Start => table.invoke_start(dev, irp),
        IrpMajor::Stop => table.invoke_stop(dev, irp),
        IrpMajor::Configure => table.invoke_configure(dev, irp),
        IrpMajor::Remove => table.invoke_remove(dev, irp)
    }
}

fn bypasses_started_guard(major: IrpMajor) -> bool {
    matches!(major, IrpMajor::Start | IrpMajor::Stop | IrpMajor::Remove)
}

pub fn io_request_sync(
    dev: &DeviceObjectK,
    major: IrpMajor,
    minor: IrpMinor,
    buffer: MemoryRegion,
    offset: usize
) -> Result<Irp, KError> {
    if !bypasses_started_guard(major) && !dev.is_started() {
        return Err(KError::DeviceStopped);
    }
    let driver = dev.driver.as_ref().ok_or(KError::Unsupported)?;

    let event = KEvent::new(false);
    let mut irp = Irp::new(major, buffer, offset, Some(io_complete), &event as *const KEvent as *mut c_void);
    irp.minor_code = minor;

    let status = dispatch(driver, major, dev.device_ptr(), &mut irp);
    if status == Status::Pending {
        event.wait()?;
    } else {
        irp.status = status;
    }

    // Detach the completion hooks before handing the value back (the event is
    // about to go out of scope; the request is already complete).
    irp.completion_routine = None;
    irp.completion_ctx = core::ptr::null_mut();
    Ok(irp)
}

extern "C" fn io_complete(irp: *mut Irp, ctx: *mut c_void) {
    info!("io_complete: status_code {}", unsafe { (*irp).status as usize });
    let event = unsafe { &*(ctx as *const KEvent) };
    event.signal();
}

struct AsyncCtx {
    routine: extern "C" fn(*mut Irp, *mut c_void),
    ctx: *mut c_void,
    irp: *mut Irp
}

unsafe fn finalize_async(actx: *mut AsyncCtx) {
    let routine = unsafe { (*actx).routine };
    let ctx = unsafe { (*actx).ctx };
    let irp = unsafe { (*actx).irp };
    routine(irp, ctx);
    drop(unsafe { Box::from_raw_in(irp, PoolAllocatorGlobal) });
    drop(unsafe { Box::from_raw_in(actx, PoolAllocatorGlobal) });
}

extern "C" fn io_complete_async(_irp: *mut Irp, ctx: *mut c_void) {
    unsafe { finalize_async(ctx as *mut AsyncCtx) };
}

pub fn io_request_async(
    dev: &DeviceObjectK,
    major: IrpMajor,
    minor: IrpMinor,
    buffer: MemoryRegion,
    offset: usize,
    routine: extern "C" fn(*mut Irp, *mut c_void),
    ctx: *mut c_void
) -> Result<Status, KError> {
    if !bypasses_started_guard(major) && !dev.is_started() {
        return Err(KError::DeviceStopped);
    }
    let driver = dev.driver.as_ref().ok_or(KError::Unsupported)?;

    let actx = Box::into_raw_with_allocator(Box::new_in(
        AsyncCtx { routine, ctx, irp: core::ptr::null_mut() },
        PoolAllocatorGlobal
    )).0;

    let mut irp_box = Box::new_in(
        Irp::new(major, buffer, offset, Some(io_complete_async), actx as *mut c_void),
        PoolAllocatorGlobal
    );
    irp_box.minor_code = minor;
    let irp_raw = Box::into_raw_with_allocator(irp_box).0;
    unsafe { (*actx).irp = irp_raw; }

    let status = dispatch(driver, major, dev.device_ptr(), irp_raw);
    if status == Status::Pending {
        Ok(Status::Pending)
    } else {
        // Driver completed synchronously without calling complete_irp.
        unsafe {
            (*irp_raw).status = status;
            finalize_async(actx);
        }
        Ok(status)
    }
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
            config_sem: KSem::new(1, 1)
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
    if let Some(driver) = DRIVER_BY_NAME.lock().get(name) {
        return Ok(driver.clone());
    }
    let path = format!("/sys/drivers/lib{}.so", name);
    let driver = load_driver(&path)?;
    DRIVER_BY_NAME.lock().insert(name.to_string(), driver.clone());
    Ok(driver)
}

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
            config_sem: KSem::new(1, 1)
        },
        PoolAllocatorGlobal
    );

    let device_ptr = device.device_ptr() as *mut DeviceObject;
    DEVICE_REGISTRY.lock().insert(id, device);

    // Attach the new device under its parent (if any) so io can discover it.
    if let Some(pid) = parent_id {
        if let Some(parent_dev) = get_device(pid) {
            parent_dev.children.lock().push(id);
        }
    }

    info!("io_create_device: device {} (driver {}) parent {:?}", id, driver_id, parent_id);
    device_ptr
}

fn resolve_device(ptr: *const DeviceObject) -> Option<DeviceHandleK> {
    if ptr.is_null() {
        return None;
    }
    get_device(unsafe { (*ptr).id })
}

// Tear down a device subtree: remove children first, stop self, dispatch Remove
// then detach and drop the device object.
pub fn remove_device(dev: &DeviceObjectK) {
    for cid in dev.children_snapshot() {
        if let Some(child) = get_device(cid) {
            remove_device(&child);
        }
    }

    {
        let _g = dev.config_guard();
        dev.stop_self();
        let _ = io_request_sync(dev, IrpMajor::Remove, IrpMinor::None, EMPTY_REGION, 0);
    }

    if let Some(pid) = dev.parent_id() {
        if let Some(parent) = get_device(pid) {
            parent.children.lock().retain(|&c| c != dev.id);
        }
    }
    DEVICE_REGISTRY.lock().remove(&dev.id);
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
            stack::enumerate_and_detect(dev);
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
    completion: Option<extern "C" fn(*mut Irp, *mut c_void)>,
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
        None => io_request_sync(&dev, major, minor, buffer, offset).map(|irp| irp.status),
        Some(routine) => io_request_async(&dev, major, minor, buffer, offset, routine, completion_ctx)
    };

    result.unwrap_or(Status::Failed)
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
    ROOT_DEVICE.call_once(create_root_device);

    // Parse boot.conf and bring up every Root stack. Enumeration inside the
    // root bus drivers recursively detects and loads child stacks.
    stack::load_boot_config();
    stack::load_root_stacks();

    // Try a read and a recursive stop on the first root device.
    let root = root_device();
    let dev_m = root.children_snapshot().first().copied().and_then(get_device);
    let dev_m = match dev_m {
        Some(dev) => dev,
        None => {
            info!("io::init: no root device was created");
            return;
        }
    };

    info!("io::init: issuing read to '{}'", dev_m.name());
    let status = dev_m.read(ReadRequest { buffer: EMPTY_REGION, offset: 0 });
    info!("io::init: read returned {:?}", status.map(|s| s as isize));

    // Exercise re-enumeration: the bus reshuffles its PDOs; io reconciles
    // (one child subtree removed, one added & detected).
    info!("io::init: invalidating '{}' to re-enumerate", dev_m.name());
    io_invalidate_device(dev_m.device_ptr());

    // Retry any stacks that failed to load (no-op on the happy path).
    info!("io::init: Trying refresh");
    stack::refresh_device_tree();

    info!("io::init: stopping device tree rooted at '{}'", dev_m.name());
    let _ = dev_m.stop();
    info!("io::init: stop complete");
}
