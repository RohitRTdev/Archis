mod osl;
mod table;

pub use table::*;
use kernel_intf::info;
use acpi_intf::*;

pub fn init() {
    // Bring the OSL up first — ACPICA's AcpiOs* calls during the subsystem
    // bring-up (cache creation, mutex creation, table scanning) need the
    // work queue and bookkeeping ready.
    osl::init();

    unsafe {
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