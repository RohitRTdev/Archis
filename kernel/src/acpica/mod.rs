mod osl;
mod table;

use core::ffi::{c_void, c_char};
use kernel_intf::info;
pub use table::*;
pub use acpi_intf::*;

unsafe extern "C" {
    fn AcpiInitializeSubsystem() -> ACPI_STATUS;
    fn AcpiInitializeTables(initial_storage: *mut c_void, initial_table_count: u32, allow_resize: u8) -> ACPI_STATUS;
    fn AcpiLoadTables() -> ACPI_STATUS;
    fn AcpiEnableSubsystem(flags: u32) -> ACPI_STATUS;
    fn AcpiInitializeObjects(flags: u32) -> ACPI_STATUS;
    fn AcpiEnterSleepStatePrep(sleep_state: u8) -> ACPI_STATUS;
    fn AcpiEnterSleepState(sleep_state: u8) -> ACPI_STATUS;

    fn AcpiGetDevices(
        hid: *const c_char,
        user_function: Option<AcpiWalkCallback>,
        context: *mut c_void,
        return_value: *mut *mut c_void
    ) -> ACPI_STATUS;

    fn AcpiGetObjectInfo(object: *mut c_void, return_buffer: *mut *mut u8) -> ACPI_STATUS;
    fn AcpiGetCurrentResources(device_handle: *mut c_void, ret_buffer: *mut AcpiBufferRaw) -> ACPI_STATUS;
    fn AcpiOsFree(memory: *mut c_void);
}


#[unsafe(no_mangle)]
extern "C" fn acpica_init() {
    unsafe {
        osl::init();

        info!("Initializing ACPI subsystem");
        let status = AcpiInitializeSubsystem();
        assert_eq!(status, AE_OK);

        info!("Initializing ACPI tables");
        let status = AcpiInitializeTables(core::ptr::null_mut(), 16, 1);
        assert_eq!(status, AE_OK);

        info!("Loading ACPI tables");
        let status = AcpiLoadTables();
        assert_eq!(status, AE_OK);

        info!("Enabling ACPI Subsystem");
        let status = AcpiEnableSubsystem(0);
        assert_eq!(status, AE_OK);

        info!("Initializing ACPI objects");
        let status = AcpiInitializeObjects(0);
        assert_eq!(status, AE_OK);

        info!("ACPICA fully initialised");
    }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_enter_sleep_state_prep_ffi(sleep_state: u8) -> ACPI_STATUS {
    unsafe { AcpiEnterSleepStatePrep(sleep_state) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_enter_sleep_state_ffi(sleep_state: u8) -> ACPI_STATUS {
    unsafe { AcpiEnterSleepState(sleep_state) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_get_devices_ffi(
    hid: *const c_char,
    user_function: Option<AcpiWalkCallback>,
    context: *mut c_void,
    return_value: *mut *mut c_void
) -> ACPI_STATUS {
    unsafe {AcpiGetDevices(hid, user_function, context, return_value) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_enumerate_devices_ffi(cb: AcpiWalkCallback, ctx: *mut c_void) -> ACPI_STATUS {
    unsafe { AcpiGetDevices(core::ptr::null(), Some(cb), ctx, core::ptr::null_mut()) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_get_object_info_ffi(handle: *mut c_void, ret: *mut *mut u8) -> ACPI_STATUS {
    unsafe { AcpiGetObjectInfo(handle, ret) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_os_free_ffi(ptr: *mut c_void) {
    unsafe { AcpiOsFree(ptr) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_get_current_resources_ffi(handle: *mut c_void, ret_buf: *mut AcpiBufferRaw) -> ACPI_STATUS {
    unsafe { AcpiGetCurrentResources(handle, ret_buf) }
}
