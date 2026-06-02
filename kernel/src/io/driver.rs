use alloc::sync::Arc;
use kernel_intf::driver::Status;
use kernel_intf::list::{DynList, List};
use kernel_intf::{info, KError};
use kernel_intf::driver::{DeviceObject, Irp, DriverObject};
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::loader::{LoadedImage, load_image};
use crate::sync::Spinlock;

static DRIVER_REGISTRY: Spinlock<DynList<Arc<Spinlock<DriverObjectK>, PoolAllocatorGlobal>>> = Spinlock::new(List::new());

struct DriverObjectK {
    image: LoadedImage,
    driver: DriverObject
}

fn load_driver(path: &str) -> Result<(), KError> {
    let img = load_image(path, false)?;
    let (driver, init) = {
        let guard = img.lock();
        let file_guard = guard.file_handle.as_ref().unwrap().lock();
        let file_path = file_guard.get_path();
        let file_base_name = file_path.get_file_stem();
        let driver = DriverObjectK {
            image: img.clone(),
            driver: DriverObject::new(file_base_name)
        };
        let driver_obj = Arc::new_in(Spinlock::new(driver), PoolAllocatorGlobal);
        let entry_addr = guard.info.entry;
        (driver_obj, unsafe { core::mem::transmute::<_, extern "C" fn(*const DriverObject) -> Status>(entry_addr) })
    };

    {
        let driver_ptr = &driver.lock().driver as *const _;

        if init(driver_ptr) == Status::Failed {
            info!("Driver init for {} failed!", path);
            return Err(KError::DriverLoadFailed);
        }
    }

    DRIVER_REGISTRY.lock().add_node(driver)?;

    Ok(())
}

pub fn init() {
    load_driver("/sys/drivers/libtest1.so").expect("Failed to load driver");
}

pub fn submit_read() {
    let driver_arc: *const DriverObjectK = {
        let registry = DRIVER_REGISTRY.lock();
        let guard = registry.first().unwrap().lock();
        &*guard as *const _
    };

    let dev = DeviceObject::new();
    let irp = Irp::new();

    unsafe {
        (*driver_arc).driver.dispatch.read(&dev as *const _, &irp as *const _);
    }
}