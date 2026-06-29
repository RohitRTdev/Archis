#![cfg_attr(not(test), no_std)]
#![allow(non_camel_case_types)]

use core::ffi::{c_void, c_char};

pub type ACPI_STATUS = u32;
pub type ACPI_PHYSICAL_ADDRESS = u64;
pub type ACPI_THREAD_ID = u64;
pub type ACPI_SIZE = usize;
pub type ACPI_STRING = *const c_char;
pub type ACPI_OSD_EXEC_CALLBACK = extern "C" fn(*mut c_void);

pub type AcpiHandle = *mut c_void;
pub type AcpiObjectType = u32;

#[repr(C)]
pub struct AcpiPnpDeviceId {
    pub length: u32,
    pub string: *const i8
}

#[repr(C)]
pub struct AcpiPnpDeviceIdList {
    count: u32,
    list_size: u32 
}

pub type AcpiWalkCallback = unsafe extern "C" fn(
    handle: AcpiHandle,
    nesting_level: u32,
    context: *mut c_void,
    return_value: *mut *mut c_void
) -> ACPI_STATUS;

pub type AcpiAddrSpaceHandler = unsafe extern "C" fn(
    function: u32,
    address: ACPI_PHYSICAL_ADDRESS,
    bit_width: u32,
    value: *mut u64,
    handler_ctx: *mut c_void,
    region_ctx: *mut c_void
) -> ACPI_STATUS;

pub type AcpiAddrSpaceSetup = unsafe extern "C" fn(
    region_handle: *mut c_void,
    function: u32,
    handler_ctx: *mut c_void,
    region_ctx: *mut *mut c_void
) -> ACPI_STATUS;

pub type AcpiNotifyHandler = unsafe extern "C" fn(
    device: AcpiHandle,
    value: u32,
    context: *mut c_void
);

// Returns ACPI_REENABLE_GPE to re-arm the GPE, or 0 to leave it masked.
pub type AcpiGpeHandler = unsafe extern "C" fn(
    device: AcpiHandle,
    gpe_number: u32,
    context: *mut c_void
) -> u32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AcpiSimpleResource {
    pub res_type: u32,
    pub address: u64,
    pub length: u64,
    pub active_high: bool,
    pub edge_triggered: bool
}

#[repr(C)]
pub struct ACPI_PREDEFINED_NAMES {
    name: *const c_char,
    type_acpi: u8,
    val: *mut c_char
}

#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct AcpiTableHeader {
    signature: [u8; ACPI_NAMESEG_SIZE],      
    pub length: u32,                            
    revision: u8, 
    checksum: u8,  
    oem_id: [u8; ACPI_OEM_ID_SIZE],
    oem_table_id: [u8; ACPI_OEM_TABLE_ID_SIZE],
    oem_rev: u32,
    asl_compiler_id: [u8; ACPI_NAMESEG_SIZE],
    asl_compiler_rev: u32
}

#[repr(C)]
pub struct AcpiPciId {
    pub segment: u16,
    pub bus: u16,
    pub device: u16,
    pub function: u16
}

#[derive(Debug)]
#[repr(usize)]
pub enum AcpiAddressType {
    SYSTEM_MEMORY,
    SYSTEM_IO,
    PCI_CONFIG
}

#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct AcpiGenericAddress {
    pub space_id: u8,
    pub bit_width: u8,
    pub bit_offset: u8,
    pub access_width: u8,
    pub address: u64
}

#[repr(C)]
pub struct AcpiBufferRaw {
    pub length: usize,
    pub pointer: *mut c_void
}

pub const ACPI_ALLOCATE_BUFFER: usize = usize::MAX;

#[cfg_attr(not(feature = "link-kernel"), link(name = "aris"))]
unsafe extern "C" {
    pub fn acpica_init();

    fn acpi_enter_sleep_state_prep_ffi(sleep_state: u8) -> ACPI_STATUS;
    fn acpi_enter_sleep_state_ffi(sleep_state: u8) -> ACPI_STATUS;

    fn acpi_enumerate_devices_ffi(cb: AcpiWalkCallback, ctx: *mut c_void) -> ACPI_STATUS;
    fn acpi_get_devices_ffi(
            hid: *const c_char,
            user_function: Option<AcpiWalkCallback>,
            context: *mut c_void,
            return_value: *mut *mut c_void
        ) -> ACPI_STATUS;

    fn acpi_get_object_info_ffi(handle: AcpiHandle, ret: *mut *mut u8) -> ACPI_STATUS;
    fn acpi_os_free_ffi(ptr: *mut c_void);
    fn acpi_get_current_resources_ffi(handle: AcpiHandle, ret_buf: *mut AcpiBufferRaw) -> ACPI_STATUS;
    fn acpi_get_parent_ffi(object: AcpiHandle, out_handle: *mut AcpiHandle) -> ACPI_STATUS;
    fn acpi_get_irq_routing_table_ffi(device: AcpiHandle, ret_buf: *mut AcpiBufferRaw) -> ACPI_STATUS;
    fn acpi_get_handle_ffi(parent: AcpiHandle, pathname: *const c_char, ret_handle: *mut AcpiHandle) -> ACPI_STATUS;
    fn acpi_evaluate_integer_ffi(object: AcpiHandle, pathname: *const c_char, return_value: *mut u64) -> ACPI_STATUS;

    fn acpi_install_address_space_handler_ffi(
        device: AcpiHandle,
        space_id: u8,
        handler: Option<AcpiAddrSpaceHandler>,
        setup: Option<AcpiAddrSpaceSetup>,
        context: *mut c_void,
    ) -> ACPI_STATUS;

    fn acpi_install_notify_handler_ffi(
        device: AcpiHandle,
        handler_type: u32,
        handler: Option<AcpiNotifyHandler>,
        context: *mut c_void,
    ) -> ACPI_STATUS;

    fn acpi_remove_address_space_handler_ffi(
        device: AcpiHandle,
        space_id: u8,
        handler: Option<AcpiAddrSpaceHandler>,
    ) -> ACPI_STATUS;

    fn acpi_remove_notify_handler_ffi(
        device: AcpiHandle,
        handler_type: u32,
        handler: Option<AcpiNotifyHandler>,
    ) -> ACPI_STATUS;

    fn acpi_install_gpe_handler_ffi(
        device: AcpiHandle,
        gpe_number: u32,
        gpe_type: u32,
        handler: Option<AcpiGpeHandler>,
        context: *mut c_void,
    ) -> ACPI_STATUS;

    fn acpi_remove_gpe_handler_ffi(
        device: AcpiHandle,
        gpe_number: u32,
        handler: Option<AcpiGpeHandler>,
    ) -> ACPI_STATUS;

    fn acpi_enable_gpe_ffi(device: AcpiHandle, gpe_number: u32) -> ACPI_STATUS;
    fn acpi_disable_gpe_ffi(device: AcpiHandle, gpe_number: u32) -> ACPI_STATUS;
    fn acpi_evaluate_void_ffi(object: AcpiHandle, pathname: *const c_char) -> ACPI_STATUS;
}

pub fn acpi_enter_sleep_state_prep(sleep_state: u8) -> ACPI_STATUS {
    unsafe { acpi_enter_sleep_state_prep_ffi(sleep_state) }
}

pub fn acpi_enter_sleep_state(sleep_state: u8) -> ACPI_STATUS {
    unsafe { acpi_enter_sleep_state_ffi(sleep_state) }
}

pub fn acpi_enumerate_devices(cb: AcpiWalkCallback, ctx: *mut c_void) -> ACPI_STATUS {
    unsafe { acpi_enumerate_devices_ffi(cb, ctx) }
}

pub fn acpi_get_devices(
    hid: *const c_char,
    user_function: Option<AcpiWalkCallback>,
    context: *mut c_void,
    return_value: *mut *mut c_void
) -> ACPI_STATUS {
    unsafe {acpi_get_devices_ffi(hid, user_function, context, return_value) }
}

pub fn acpi_get_object_info(handle: AcpiHandle, ret: *mut *mut u8) -> ACPI_STATUS {
    unsafe { acpi_get_object_info_ffi(handle, ret) }
}

pub fn acpi_os_free(ptr: *mut c_void) {
    unsafe { acpi_os_free_ffi(ptr) }
}

pub fn acpi_get_current_resources(handle: AcpiHandle, ret_buf: *mut AcpiBufferRaw) -> ACPI_STATUS {
    unsafe { acpi_get_current_resources_ffi(handle, ret_buf) }
}

pub fn acpi_get_parent(object: AcpiHandle, out_handle: *mut AcpiHandle) -> ACPI_STATUS {
    unsafe { acpi_get_parent_ffi(object, out_handle) }
}

pub fn acpi_get_irq_routing_table(device: AcpiHandle, ret_buf: *mut AcpiBufferRaw) -> ACPI_STATUS {
    unsafe { acpi_get_irq_routing_table_ffi(device, ret_buf) }
}

pub fn acpi_get_handle(parent: AcpiHandle, pathname: *const c_char, ret_handle: *mut AcpiHandle) -> ACPI_STATUS {
    unsafe { acpi_get_handle_ffi(parent, pathname, ret_handle) }
}

pub fn acpi_evaluate_integer(object: AcpiHandle, pathname: *const c_char, return_value: *mut u64) -> ACPI_STATUS {
    unsafe { acpi_evaluate_integer_ffi(object, pathname, return_value) }
}

pub fn acpi_install_address_space_handler(
    device: AcpiHandle,
    space_id: u8,
    handler: Option<AcpiAddrSpaceHandler>,
    setup: Option<AcpiAddrSpaceSetup>,
    context: *mut c_void,
) -> ACPI_STATUS {
    unsafe { acpi_install_address_space_handler_ffi(device, space_id, handler, setup, context) }
}

pub fn acpi_install_notify_handler(
    device: AcpiHandle,
    handler_type: u32,
    handler: Option<AcpiNotifyHandler>,
    context: *mut c_void,
) -> ACPI_STATUS {
    unsafe { acpi_install_notify_handler_ffi(device, handler_type, handler, context) }
}

pub fn acpi_remove_address_space_handler(
    device: AcpiHandle,
    space_id: u8,
    handler: Option<AcpiAddrSpaceHandler>,
) -> ACPI_STATUS {
    unsafe { acpi_remove_address_space_handler_ffi(device, space_id, handler) }
}

pub fn acpi_remove_notify_handler(
    device: AcpiHandle,
    handler_type: u32,
    handler: Option<AcpiNotifyHandler>,
) -> ACPI_STATUS {
    unsafe { acpi_remove_notify_handler_ffi(device, handler_type, handler) }
}

pub fn acpi_install_gpe_handler(
    device: AcpiHandle,
    gpe_number: u32,
    gpe_type: u32,
    handler: Option<AcpiGpeHandler>,
    context: *mut c_void,
) -> ACPI_STATUS {
    unsafe { acpi_install_gpe_handler_ffi(device, gpe_number, gpe_type, handler, context) }
}

pub fn acpi_remove_gpe_handler(
    device: AcpiHandle,
    gpe_number: u32,
    handler: Option<AcpiGpeHandler>,
) -> ACPI_STATUS {
    unsafe { acpi_remove_gpe_handler_ffi(device, gpe_number, handler) }
}

pub fn acpi_enable_gpe(device: AcpiHandle, gpe_number: u32) -> ACPI_STATUS {
    unsafe { acpi_enable_gpe_ffi(device, gpe_number) }
}

pub fn acpi_disable_gpe(device: AcpiHandle, gpe_number: u32) -> ACPI_STATUS {
    unsafe { acpi_disable_gpe_ffi(device, gpe_number) }
}

pub fn acpi_evaluate_void(object: AcpiHandle, pathname: *const c_char) -> ACPI_STATUS {
    unsafe { acpi_evaluate_void_ffi(object, pathname) }
}

pub const AE_OK: ACPI_STATUS         = 0x0000_0000;
pub const AE_ERROR: ACPI_STATUS      = 0x0000_0001;
pub const AE_NOT_FOUND: ACPI_STATUS  = 0x0000_0005;
pub const AE_BAD_PARAMETER: ACPI_STATUS = 0x0000_1001;
pub const AE_TIME: ACPI_STATUS       = 0x0000_0011;
pub const AE_SUPPORT: ACPI_STATUS    = 0x0000_001D;
pub const AE_ALREADY_EXISTS: ACPI_STATUS = 0x0000_0007;

pub const ACPI_ALL_NOTIFY: u32 = 0x3;
pub const ACPI_GPE_LEVEL_TRIGGERED: u32 = 0x08;
pub const ACPI_REENABLE_GPE: u32 = 0x80;

// AcpiOsInstallInterruptHandler return codes (ACPICA reads these from the
// wrapper). 1 = interrupt was ours, 0 = pass through to next handler.
pub const ACPI_INTERRUPT_HANDLED: u32 = 1;

// AcpiOsSignal function codes.
pub const ACPI_SIGNAL_FATAL: u32      = 0;
pub const ACPI_SIGNAL_BREAKPOINT: u32 = 1;

// Mutex timeout sentinel — ACPICA passes this when the caller wants to wait
// forever. Anything below is interpreted as a millisecond timeout.
pub const ACPI_WAIT_FOREVER: u16 = 0xFFFF;

pub const ACPI_NAMESEG_SIZE: usize = 4;
pub const ACPI_OEM_ID_SIZE: usize = 6;
pub const ACPI_OEM_TABLE_ID_SIZE: usize = 8;

pub const ACPI_SLEEP_S5: u8 = 5;
pub const ACPI_ADR_SPACE_EC: u8 = 3;

// ACPI TABLES
pub trait AcpiTable {
    const TABLE_NAME: &'static str;
}

#[derive(Debug)]
#[repr(C, packed)]
pub struct AcpiTableHpet {
    pub header: AcpiTableHeader,   
    pub event_timer_block_id: u32,   
    pub address: AcpiGenericAddress, 
    pub hpet_number: u8,             
    pub min_tick: u16,               
    pub flags: u8                   
}

#[derive(Debug)]
#[repr(C, packed)]
pub struct AcpiTableMadt {
    pub header: AcpiTableHeader,
    pub con_addr: u32,
    pub flags: u32
}

impl AcpiTable for AcpiTableHpet {
    const TABLE_NAME: &'static str = "HPET";
}

impl AcpiTable for AcpiTableMadt {
    const TABLE_NAME: &'static str = "APIC";
}

#[derive(Debug)]
#[repr(C, packed)]
pub struct AcpiTableEcdt {
    pub header: AcpiTableHeader,
    pub control: AcpiGenericAddress, // EC command/status port
    pub data: AcpiGenericAddress,    // EC data port
    pub uid: u32,
    pub gpe: u8
}

impl AcpiTable for AcpiTableEcdt {
    const TABLE_NAME: &'static str = "ECDT";
}
