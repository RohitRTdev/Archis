mod osl;
mod table;

use core::ffi::c_void;
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

