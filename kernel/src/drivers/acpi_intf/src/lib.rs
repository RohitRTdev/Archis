#![no_std]
#![allow(non_camel_case_types)]

use core::ffi::{c_void, c_char};

pub type ACPI_STATUS = u32;
pub type ACPI_PHYSICAL_ADDRESS = u64;
pub type ACPI_THREAD_ID = u64;
pub type ACPI_SIZE = usize;
pub type ACPI_STRING = *const c_char;
pub type ACPI_OSD_EXEC_CALLBACK = extern "C" fn(*mut c_void);

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

unsafe extern "C" {
    // Init
    pub fn AcpiInitializeSubsystem() -> ACPI_STATUS;
    pub fn AcpiInitializeTables(initial_storage: *mut c_void, initial_table_count: u32, allow_resize: u8) -> ACPI_STATUS;
    pub fn AcpiLoadTables() -> ACPI_STATUS;
    pub fn AcpiEnableSubsystem(flags: u32) -> ACPI_STATUS;
    pub fn AcpiInitializeObjects(flags: u32) -> ACPI_STATUS;

    // Sleep
    fn AcpiEnterSleepStatePrep(sleep_state: u8) -> ACPI_STATUS;  
    fn AcpiEnterSleepState(sleep_state: u8) -> ACPI_STATUS;
}


pub fn acpi_enter_sleep_state_prep(sleep_state: u8) -> ACPI_STATUS {
    unsafe { AcpiEnterSleepStatePrep(sleep_state) }
}  

pub fn acpi_enter_sleep_state(sleep_state: u8) -> ACPI_STATUS {
    unsafe { AcpiEnterSleepState(sleep_state) }
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
