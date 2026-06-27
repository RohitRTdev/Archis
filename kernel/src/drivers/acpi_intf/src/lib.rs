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

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AcpiSimpleResource {
    pub res_type: u32,
    pub address: u64,
    pub length: u64
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

#[cfg_attr(not(feature = "link-kernel"), link(name = "aris"))]
unsafe extern "C" {
    pub fn acpica_init();

    fn acpi_enter_sleep_state_prep_ffi(sleep_state: u8) -> ACPI_STATUS;
    fn acpi_enter_sleep_state_ffi(sleep_state: u8) -> ACPI_STATUS;

    fn acpi_enumerate_devices_ffi(cb: AcpiWalkCallback, ctx: *mut c_void) -> ACPI_STATUS;
    fn acpi_get_hid_ffi(handle: AcpiHandle, buf: *mut u8, len: usize) -> usize;
    fn acpi_get_resources_ffi(handle: AcpiHandle, out: *mut AcpiSimpleResource, max: usize) -> usize;
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

pub fn acpi_get_hid(handle: AcpiHandle, buf: &mut [u8]) -> usize {
    unsafe { acpi_get_hid_ffi(handle, buf.as_mut_ptr(), buf.len()) }
}

pub fn acpi_get_resources(handle: AcpiHandle, out: *mut AcpiSimpleResource, max: usize) -> usize {
    unsafe { acpi_get_resources_ffi(handle, out, max) }
}

pub const AE_OK: ACPI_STATUS         = 0x0000_0000;
pub const AE_ERROR: ACPI_STATUS      = 0x0000_0001;
pub const AE_NOT_FOUND: ACPI_STATUS  = 0x0000_0005;
pub const AE_BAD_PARAMETER: ACPI_STATUS = 0x0000_1001;
pub const AE_TIME: ACPI_STATUS       = 0x0000_0011;
pub const AE_SUPPORT: ACPI_STATUS    = 0x0000_001D;

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
