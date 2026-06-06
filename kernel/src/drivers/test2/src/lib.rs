#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use core::ffi::c_void;

use kernel_intf::info;
use kernel_intf::driver::{DeviceObject, DriverObject, Irp, IrpMinor, Status, create_device};
use kernel_intf::mem::PoolAllocatorGlobal;

struct Test2Device {
    _x: u64
}

#[kmod::init(driver)]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("{} initializing (id={})...", driver.get_name(), driver.id);

    kmod::dispatch_init!(
        driver,
        dispatch_add,
        dispatch_pnp,
        dispatch_read
    );

    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_add(driver: &DriverObject, pdo: Option<&DeviceObject>) -> Status {
    info!("test2 add_device: creating function FDO on PDO");

    let ctx = alloc::boxed::Box::new_in(Test2Device { _x: 0 }, PoolAllocatorGlobal);
    let ctx_ptr = alloc::boxed::Box::into_raw_with_allocator(ctx).0 as *mut c_void;

    let dev = create_device(driver, Some("test2"), ctx_ptr, pdo);
    if dev.is_null() {
        info!("test2 add_device: create_device failed");
        return Status::Failed;
    }
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_pnp(device: &DeviceObject, request: &mut Irp) -> Status {
    match request.minor_code {
        IrpMinor::Start => {
            dispatch_start(device, request)
        },
        IrpMinor::Stop => {
            dispatch_stop(device, request)
        },
        IrpMinor::Remove => {
            dispatch_remove(device, request)
        },
        _ => {
            Status::Unsupported
        }
    }
}


fn dispatch_start(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("test2 start_device {}", device.id);
    request.complete_irp(Status::Success);
    Status::Success
}

fn dispatch_stop(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("test2 stop_device {}", device.id);
    request.complete_irp(Status::Success);
    Status::Success
}

fn dispatch_remove(device: &DeviceObject, request: &mut Irp) -> Status {
    info!("test2 remove_device {}", device.id);
    if !device.ctx.is_null() {
        unsafe {
            drop(alloc::boxed::Box::from_raw_in(device.ctx as *mut Test2Device, PoolAllocatorGlobal));
        }
    }
    request.complete_irp(Status::Success);
    Status::Success
}

#[kmod::dispatch_handler]
fn dispatch_read(_device: &DeviceObject, request: &mut Irp) -> Status {
    info!("test2 read");
    request.complete_irp(Status::Success);
    Status::Success
}

#[kmod::export]
fn get_test2() {
    kernel_intf::info!("Calling get_test2");
}
