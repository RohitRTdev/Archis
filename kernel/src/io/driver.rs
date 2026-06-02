use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use common::{MemoryRegion, StrRef};
use kernel_intf::driver::{DeviceObject, DriverObject, Irp, IrpMajor, Status};
use kernel_intf::list::{DynList, List};
use kernel_intf::mem::PoolAllocatorGlobal;
use kernel_intf::{KError, info};

use crate::loader::{LoadedImage, load_image};
use crate::sync::{KEvent, Spinlock};

type DriverHandle = Arc<DriverObjectK, PoolAllocatorGlobal>;
pub type DeviceHandle = Arc<DeviceObjectK, PoolAllocatorGlobal>;

#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum DeviceState {
    Stopped = 0,
    Started = 1
}

struct DriverObjectK {
    _image: LoadedImage,
    driver: DriverObject
}

pub struct DeviceObjectK {
    device: DeviceObject,
    driver: DriverHandle,
    state: AtomicUsize
}

impl DeviceObjectK {
    fn device_ptr(&self) -> *const DeviceObject {
        &self.device as *const DeviceObject
    }

    // Bring the device up via its driver's Start handler.
    pub fn start(&self) -> Result<Status, KError> {
        let mut irp = Irp::new(
            IrpMajor::Start,
            MemoryRegion { base_address: 0, size: 0 },
            0,
            None,
            core::ptr::null_mut()
        );

        let status = self.driver.driver.dispatch.invoke_start(self.device_ptr(), &mut irp);
        if status == Status::Success {
            self.state.store(DeviceState::Started as usize, Ordering::Release);
        }

        Ok(status)
    }

    pub fn read(&self, req: ReadRequest) -> Result<Status, KError> {
        if self.state.load(Ordering::Acquire) != DeviceState::Started as usize {
            info!("read() on a device that is not started");
            return Err(KError::Unsupported);
        }

        let event = KEvent::new(false);

        let mut irp = Box::new_in(
            Irp::new(
                IrpMajor::Read,
                req.buffer,
                req.offset,
                Some(io_complete),
                &event as *const KEvent as *mut c_void
            ),
            PoolAllocatorGlobal
        );

        let status = self.driver.driver.dispatch.invoke_read(self.device_ptr(), &mut *irp);

        if status == Status::Pending {
            // The driver completes asynchronously; block until it signals us.
            info!("Waiting on pending state...");
            event.wait()?;
        } else {
            irp.status = status;
        }
        info!("Completed read!");

        Ok(irp.status)
    }
}

pub struct ReadRequest {
    pub buffer: MemoryRegion,
    pub offset: usize
}

extern "C" fn io_complete(irp: *mut Irp, ctx: *mut c_void) {
    info!("Called io_complete... with status_code: {}", unsafe{(*irp).status as usize});
    let event = unsafe { &*(ctx as *const KEvent) };
    event.signal();
}

// Exported to drivers: spawn a kernel worker thread. The handler must never
// return; it should finish by calling `exit_kernel_thread`. This is a stopgap
// for letting a driver complete a pending IRP from another context until a
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

static NEXT_DRIVER_ID: AtomicUsize = AtomicUsize::new(0);
static DRIVER_REGISTRY: Spinlock<BTreeMap<usize, DriverHandle>> = Spinlock::new(BTreeMap::new());
static DEVICE_REGISTRY: Spinlock<DynList<DeviceHandle>> = Spinlock::new(List::new());

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

#[unsafe(no_mangle)]
pub extern "C" fn io_create_device(driver_id: usize, name: StrRef, ctx: *mut c_void) -> *mut DeviceObject {
    let driver = match DRIVER_REGISTRY.lock().get(&driver_id) {
        Some(driver) => driver.clone(),
        None => {
            info!("io_create_device: unknown driver id {}", driver_id);
            return core::ptr::null_mut();
        }
    };

    let device = Arc::new_in(
        DeviceObjectK {
            device: DeviceObject::new(name, ctx),
            driver,
            state: AtomicUsize::new(DeviceState::Stopped as usize)
        },
        PoolAllocatorGlobal
    );

    let device_ptr = device.device_ptr() as *mut DeviceObject;

    DEVICE_REGISTRY.lock().add_node(device).expect("Failed to register device object");

    info!("io_create_device: created device for driver id {}", driver_id);
    device_ptr
}

pub fn init() {
    let driver = load_driver("/sys/drivers/libtest1.so")
    .expect("Failed to load driver");

    // No bus exists for this virtual device, so the kernel plays the synthetic
    // enumerator: invoke the driver's add handler so it creates its own FDO.
    let status = driver.driver.dispatch.invoke_add(&driver.driver, core::ptr::null());
    if status != Status::Success {
        info!("Driver add_device failed");
        return;
    }

    let device = {
        let registry = DEVICE_REGISTRY.lock();
        registry.first().map(|node| (**node).clone())
    };

    let device = match device {
        Some(device) => device,
        None => {
            info!("Driver did not create a device");
            return;
        }
    };

    device.start().expect("Failed to start device");

    let status = device.read(ReadRequest {
        buffer: MemoryRegion { base_address: 0, size: 0 },
        offset: 0
    }).expect("Device read failed");

    info!("Device read completed with status code {}", status as isize);
}
