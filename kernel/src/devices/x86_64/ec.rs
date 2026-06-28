use core::ffi::c_void;
use core::sync::atomic::{AtomicU16, Ordering};

use acpi_intf::{AcpiTableEcdt, ACPI_PHYSICAL_ADDRESS, ACPI_STATUS, AE_OK};
use kernel_intf::info;

use crate::BOOT_INFO;
use crate::acpica::fetch_acpi_table;
use crate::hal;

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

// Spin until EC input buffer empty (IBF bit 1 clear), meaning EC is ready to
// accept a command or data byte. Returns false on timeout.
fn ec_wait_ibf(cmd: u16) -> bool {
    for _ in 0..0x10000 {
        let status = unsafe { hal::read_port_u8(cmd) };
        if (status & 0x02) == 0 {
            return true;
        }
    }
    false
}

// Spin until EC output buffer full (OBF bit 0 set), meaning a response byte
// is waiting in the data port. Returns false on timeout.
fn ec_wait_obf(cmd: u16) -> bool {
    for _ in 0..0x10000 {
        let status = unsafe { hal::read_port_u8(cmd) };
        if (status & 0x01) != 0 {
            return true;
        }
    }
    false
}

fn ec_read(data: u16, cmd: u16, offset: u8) -> u8 {
    if !ec_wait_ibf(cmd) { return 0; }
    unsafe { hal::write_port_u8(cmd, 0x80); } // READ command
    if !ec_wait_ibf(cmd) { return 0; }
    unsafe { hal::write_port_u8(data, offset); }
    if !ec_wait_obf(cmd) { return 0; }
    unsafe { hal::read_port_u8(data) }
}

fn ec_write(data: u16, cmd: u16, offset: u8, value: u8) {
    if !ec_wait_ibf(cmd) { return; }
    unsafe { hal::write_port_u8(cmd, 0x81); } // WRITE command
    if !ec_wait_ibf(cmd) { return; }
    unsafe { hal::write_port_u8(data, offset); }
    if !ec_wait_ibf(cmd) { return; }
    unsafe { hal::write_port_u8(data, value); }
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

    // ACPI read
    if function == 0 {
        let mut result = 0u64;
        for i in 0..bytes {
            let byte = ec_read(data, cmd, (address + i) as u8) as u64;
            result |= byte << (i * 8);
        }
        unsafe { *value = result; }
    } else {
        // ACPI write
        let v = unsafe { *value };
        for i in 0..bytes {
            ec_write(data, cmd, (address + i) as u8, (v >> (i * 8)) as u8);
        }
    }

    AE_OK
}
