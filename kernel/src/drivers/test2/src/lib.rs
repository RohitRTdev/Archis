#![cfg_attr(not(test), no_std)]

use kernel_intf::driver::{Status, DriverObject};

#[kmod::init]
fn driver_init(driver: &DriverObject) -> Status {
    kernel_intf::info!("Initializing {}...", driver.get_name());
    unsafe {kernel_intf::exported_function();}

    Status::Success
}

#[kmod::export]
fn get_test2() {
    kernel_intf::info!("Calling get_test2");
}
