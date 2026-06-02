#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]

use kernel_intf::{info, exported_function};
use kernel_intf::driver::{Status, DriverObject, Irp, DeviceObject};
use alloc::vec::Vec;

#[kmod::init]
fn driver_init(driver: &mut DriverObject) -> Status {
    info!("{} initializing...", driver.get_name());

    kmod::dispatch_init!(driver, dispatch_read, dispatch_write);    

    let mut vec: Vec<i32, kernel_intf::mem::PoolAllocatorGlobal> = Vec::new_in(kernel_intf::mem::PoolAllocatorGlobal);

    for i in 0..10 {
        vec.push(i);
    }
    info!("{:?}", vec);

    unsafe {exported_function();test2::get_test2()};

    Status::Success
}


#[kmod::handler]
fn dispatch_read(device: &DeviceObject, request: &Irp) -> Status {

    info!("Calling driver read");
    Status::Success
}

#[kmod::handler]
fn dispatch_write(device: &DeviceObject, request: &Irp) -> Status {

    info!("Calling driver write");

    Status::Success
}
