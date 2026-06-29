use core::ffi::c_void;
use core::sync::atomic::{AtomicU16, Ordering};

use acpi_intf::{AcpiTableEcdt, ACPI_PHYSICAL_ADDRESS, ACPI_STATUS, AE_OK};
use kernel_intf::info;
use kernel_intf::hw::{ec_read, ec_write};

use crate::BOOT_INFO;
use crate::acpica::fetch_acpi_table;

static EC_DATA_PORT: AtomicU16 = AtomicU16::new(0);
static EC_CMD_PORT:  AtomicU16 = AtomicU16::new(0);

pub fn init() {
    let rsdp = match BOOT_INFO.get() {
        Some(bi) => bi.rsdp as *const u8,
        None => {
            info!("EC: BOOT_INFO not available, skipping EC init");
            return;
        }
    };

    let ecdt = match fetch_acpi_table::<AcpiTableEcdt>(rsdp) {
        Some(t) => t,
        None => {
            info!("EC: no ECDT table found, EC region handler will not be installed");
            return;
        }
    };

    let cmd_port  = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(ecdt.control.address)) } as u16;
    let data_port = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(ecdt.data.address)) } as u16;

    if cmd_port == 0 || data_port == 0 {
        info!("EC: ECDT has zero port addresses, skipping EC init");
        return;
    }

    EC_CMD_PORT.store(cmd_port, Ordering::Relaxed);
    EC_DATA_PORT.store(data_port, Ordering::Relaxed);

    info!("EC: configured from ECDT — data={:#X} cmd={:#X}", data_port, cmd_port);
}

pub fn is_available() -> bool {
    EC_CMD_PORT.load(Ordering::Relaxed) != 0
}

pub unsafe extern "C" fn ec_region_handler(
    function: u32,
    address: ACPI_PHYSICAL_ADDRESS,
    bit_width: u32,
    value: *mut u64,
    _handler_ctx: *mut c_void,
    _region_ctx: *mut c_void
) -> ACPI_STATUS {
    let data  = EC_DATA_PORT.load(Ordering::Relaxed);
    let cmd   = EC_CMD_PORT.load(Ordering::Relaxed);
    let bytes = (bit_width / 8) as u64;

    if function == 0 {
        let mut result = 0u64;
        for i in 0..bytes {
            let byte = ec_read(data, cmd, (address + i) as u8) as u64;
            result |= byte << (i * 8);
        }
        unsafe { *value = result; }
    } else {
        let v = unsafe { *value };
        for i in 0..bytes {
            ec_write(data, cmd, (address + i) as u8, (v >> (i * 8)) as u8);
        }
    }

    AE_OK
}
