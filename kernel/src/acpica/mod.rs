mod osl;
mod table;

use core::ffi::{c_void, c_char};
use kernel_intf::info;
use crate::devices::ec;
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
    fn AcpiReset() -> ACPI_STATUS;

    fn AcpiGetDevices(
        hid: *const c_char,
        user_function: Option<AcpiWalkCallback>,
        context: *mut c_void,
        return_value: *mut *mut c_void
    ) -> ACPI_STATUS;

    fn AcpiGetObjectInfo(object: *mut c_void, return_buffer: *mut *mut u8) -> ACPI_STATUS;
    fn AcpiGetCurrentResources(device_handle: *mut c_void, ret_buffer: *mut AcpiBufferRaw) -> ACPI_STATUS;
    fn AcpiGetParent(object: *mut c_void, out_handle: *mut *mut c_void) -> ACPI_STATUS;
    fn AcpiGetIrqRoutingTable(device: *mut c_void, ret_buffer: *mut AcpiBufferRaw) -> ACPI_STATUS;
    fn AcpiGetHandle(parent: *mut c_void, pathname: *const c_char, ret_handle: *mut *mut c_void) -> ACPI_STATUS;
    fn AcpiEvaluateObject(object: *mut c_void, pathname: *const c_char, params: *mut c_void, ret_buf: *mut AcpiBufferRaw) -> ACPI_STATUS;
    fn AcpiOsFree(memory: *mut c_void);

    fn AcpiInstallAddressSpaceHandler(
        device: AcpiHandle,
        space_id: u8,
        handler: Option<AcpiAddrSpaceHandler>,
        setup: Option<AcpiAddrSpaceSetup>,
        context: *mut c_void,
    ) -> ACPI_STATUS;

    fn AcpiInstallNotifyHandler(
        device: AcpiHandle,
        handler_type: u32,
        handler: Option<AcpiNotifyHandler>,
        context: *mut c_void,
    ) -> ACPI_STATUS;

    fn AcpiRemoveAddressSpaceHandler(
        device: AcpiHandle,
        space_id: u8,
        handler: Option<AcpiAddrSpaceHandler>,
    ) -> ACPI_STATUS;

    fn AcpiRemoveNotifyHandler(
        device: AcpiHandle,
        handler_type: u32,
        handler: Option<AcpiNotifyHandler>,
    ) -> ACPI_STATUS;

    fn AcpiInstallGpeHandler(
        device: AcpiHandle,
        gpe_number: u32,
        gpe_type: u32,
        handler: Option<AcpiGpeHandler>,
        context: *mut c_void,
    ) -> ACPI_STATUS;

    fn AcpiRemoveGpeHandler(
        device: AcpiHandle,
        gpe_number: u32,
        handler: Option<AcpiGpeHandler>,
    ) -> ACPI_STATUS;

    fn AcpiEnableGpe(device: AcpiHandle, gpe_number: u32) -> ACPI_STATUS;
    fn AcpiDisableGpe(device: AcpiHandle, gpe_number: u32) -> ACPI_STATUS;
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
        
        // Install EC operation region handler before AcpiInitializeObjects so
        // that _STA/_INI methods which read EC registers can execute without
        // failing and causing AcpiNsGetDeviceCallback to prune device subtrees.
        if ec::is_available() {
            let root = usize::MAX as *mut c_void;
            let status = AcpiInstallAddressSpaceHandler(
                root,
                ACPI_ADR_SPACE_EC,
                Some(ec::ec_region_handler),
                None,
                core::ptr::null_mut(),
            );
            // AE_ALREADY_EXISTS is fine if ACPICA already claimed the space
            assert!(status == AE_OK || status == AE_ALREADY_EXISTS,
                "EC region handler install failed: {:#X}", status);
            info!("EC region handler installed");
        }

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
extern "C" fn acpi_reset_ffi() -> ACPI_STATUS {
    unsafe { AcpiReset() }
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

#[unsafe(no_mangle)]
extern "C" fn acpi_get_parent_ffi(object: *mut c_void, out_handle: *mut *mut c_void) -> ACPI_STATUS {
    unsafe { AcpiGetParent(object, out_handle) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_get_irq_routing_table_ffi(device: *mut c_void, ret_buf: *mut AcpiBufferRaw) -> ACPI_STATUS {
    unsafe { AcpiGetIrqRoutingTable(device, ret_buf) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_get_handle_ffi(parent: *mut c_void, pathname: *const c_char, ret_handle: *mut *mut c_void) -> ACPI_STATUS {
    unsafe { AcpiGetHandle(parent, pathname, ret_handle) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_evaluate_integer_ffi(object: *mut c_void, pathname: *const c_char, return_value: *mut u64) -> ACPI_STATUS {
    let mut buf = AcpiBufferRaw { length: ACPI_ALLOCATE_BUFFER, pointer: core::ptr::null_mut() };
    let status = unsafe { AcpiEvaluateObject(object, pathname, core::ptr::null_mut(), &mut buf) };
    if status != AE_OK || buf.pointer.is_null() {
        return status;
    }
    let obj_type = unsafe { core::ptr::read_unaligned(buf.pointer as *const u32) };
    let result = if obj_type == 1 {
        // ACPI_TYPE_INTEGER: type(u32) + pad(u32) + value(u64) at offset 8
        unsafe { *return_value = core::ptr::read_unaligned((buf.pointer as *const u8).add(8) as *const u64); }
        AE_OK
    } else {
        AE_ERROR
    };
    unsafe { AcpiOsFree(buf.pointer); }
    result
}

#[unsafe(no_mangle)]
extern "C" fn acpi_install_address_space_handler_ffi(
    device: AcpiHandle,
    space_id: u8,
    handler: Option<AcpiAddrSpaceHandler>,
    setup: Option<AcpiAddrSpaceSetup>,
    context: *mut c_void,
) -> ACPI_STATUS {
    unsafe { AcpiInstallAddressSpaceHandler(device, space_id, handler, setup, context) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_install_notify_handler_ffi(
    device: AcpiHandle,
    handler_type: u32,
    handler: Option<AcpiNotifyHandler>,
    context: *mut c_void,
) -> ACPI_STATUS {
    unsafe { AcpiInstallNotifyHandler(device, handler_type, handler, context) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_remove_address_space_handler_ffi(
    device: AcpiHandle,
    space_id: u8,
    handler: Option<AcpiAddrSpaceHandler>,
) -> ACPI_STATUS {
    unsafe { AcpiRemoveAddressSpaceHandler(device, space_id, handler) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_remove_notify_handler_ffi(
    device: AcpiHandle,
    handler_type: u32,
    handler: Option<AcpiNotifyHandler>,
) -> ACPI_STATUS {
    unsafe { AcpiRemoveNotifyHandler(device, handler_type, handler) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_install_gpe_handler_ffi(
    device: AcpiHandle,
    gpe_number: u32,
    gpe_type: u32,
    handler: Option<AcpiGpeHandler>,
    context: *mut c_void,
) -> ACPI_STATUS {
    unsafe { AcpiInstallGpeHandler(device, gpe_number, gpe_type, handler, context) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_remove_gpe_handler_ffi(
    device: AcpiHandle,
    gpe_number: u32,
    handler: Option<AcpiGpeHandler>,
) -> ACPI_STATUS {
    unsafe { AcpiRemoveGpeHandler(device, gpe_number, handler) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_enable_gpe_ffi(device: AcpiHandle, gpe_number: u32) -> ACPI_STATUS {
    unsafe { AcpiEnableGpe(device, gpe_number) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_disable_gpe_ffi(device: AcpiHandle, gpe_number: u32) -> ACPI_STATUS {
    unsafe { AcpiDisableGpe(device, gpe_number) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_evaluate_void_ffi(object: AcpiHandle, pathname: *const c_char) -> ACPI_STATUS {
    unsafe {
        AcpiEvaluateObject(
            object,
            pathname,
            core::ptr::null_mut(),
            core::ptr::null_mut()
        )
    }
}
