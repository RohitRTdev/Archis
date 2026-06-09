use core::ptr::read_unaligned;
use acpi_intf::{AcpiTable, AcpiTableHeader};

// Public wrapper for OSL use. Takes a raw signature string (e.g. "MADT") and
// returns the physical address of the first matching table as a raw pointer.
pub fn fetch_acpi_table_raw(rsdp_ptr: *const u8, signature: &str) -> Option<*const u8> {
    fetch_acpi_table_core(rsdp_ptr, signature)
}

// These are helper table functions that can be used before/after acpica init
fn fetch_acpi_table_core(rsdp_ptr: *const u8, signature: &str) -> Option<*const u8> {
    if rsdp_ptr.is_null() {
        return None;
    }

    // Signature as bytes
    let sig = signature.as_bytes();

    unsafe {
        let xsdt = read_unaligned(rsdp_ptr.add(24) as *const u64) as usize;
        assert!(xsdt != 0, "XSDT not found with firmware provided RSDP!");

        let header = xsdt as *const AcpiTableHeader;

        // Number of entries: (total_length − header_size) / 8
        let header_len = core::mem::size_of::<AcpiTableHeader>();
        let entry_count = (read_unaligned(core::ptr::addr_of!((*header).length)) as usize - header_len) / 8;

        // Pointer to first 64-bit entry
        let entries_base = xsdt + header_len;

        for i in 0..entry_count {
            let entry_addr = entries_base + i * 8;
            let table = read_unaligned(entry_addr as *const u64);
            // Not sure if this might happen. Just for Safeguard
            if table == 0 {
                continue;
            }

            let table_ptr = table as *const u8;

            // First 4 bytes = table signature
            let table_sig = core::slice::from_raw_parts(table_ptr, 4);

            if table_sig == sig {
                return Some(table_ptr);
            }
        }
    }

    None
}


pub fn fetch_acpi_table<T: AcpiTable>(rsdt_ptr: *const u8) -> Option<&'static T> {
    fetch_acpi_table_core(rsdt_ptr, T::TABLE_NAME).and_then(|table_ptr| {
        unsafe {
            Some(&*(table_ptr as *const T))
        }
    })
}