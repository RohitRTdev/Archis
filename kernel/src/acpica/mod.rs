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

#[repr(C)]
struct AcpiBufferRaw {
    length: usize,
    pointer: *mut c_void
}

const ACPI_ALLOCATE_BUFFER: usize = usize::MAX;

// Resource type IDs from acrestyp.h
const ACPI_RESOURCE_TYPE_IRQ: u32 = 0;
const ACPI_RESOURCE_TYPE_IO: u32 = 4;
const ACPI_RESOURCE_TYPE_FIXED_IO: u32 = 5;
const ACPI_RESOURCE_TYPE_END_TAG: u32 = 7;
const ACPI_RESOURCE_TYPE_MEMORY32: u32 = 9;
const ACPI_RESOURCE_TYPE_EXTENDED_IRQ: u32 = 15;

const ACPI_VALID_HID_FLAG: u16 = 0x4;

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

#[repr(C, packed)]
struct AcpiResourceHeader {
    res_type: u32,
    length: u32
}

#[repr(C, packed)]
struct AcpiResourceSource {
    index: u8,
    string_length: u16,
    string_ptr: *const core::ffi::c_char
}

#[repr(C, packed)]
struct AcpiResourceIrq {
    descriptor_length: u8,
    triggering: u8,
    polarity: u8,
    shareable: u8,
    wake_capable: u8,
    interrupt_count: u8,
    interrupts: [u8; 1]
}

#[repr(C, packed)]
struct AcpiResourceExtendedIrq {
    producer_consumer: u8,
    triggering: u8,
    polarity: u8,
    shareable: u8,
    wake_capable: u8,
    interrupt_count: u8,
    resource_source: AcpiResourceSource,
    interrupts: [u32; 1]
}

#[repr(C, packed)]
struct AcpiResourceIo {
    io_decode: u8,
    alignment: u8,
    address_length: u8,
    minimum: u16,
    maximum: u16
}

#[repr(C, packed)]
struct AcpiResourceFixedIo {
    address: u16,
    address_length: u8
}

#[repr(C, packed)]
struct AcpiResourceMemory32 {
    write_protect: u8,
    minimum: u32,
    maximum: u32,
    alignment: u32,
    address_length: u32
}

#[repr(C)]
struct AcpiDeviceInfo {
    _info_size: u32, 
    _name: u32, 
    _type: AcpiObjectType, 
    _param_count: u8, 
    valid: u16, 
    _flags: u8, 
    _highest_d_states: [u8; 4], 
    _lowest_d_states: [u8; 5], 
    _address: u64, 
    hardware_id: AcpiPnpDeviceId, 
    _unique_id: AcpiPnpDeviceId,
    _subsystem_id: AcpiPnpDeviceId, 
    _compatible_id_list: AcpiPnpDeviceIdList 
}

#[unsafe(no_mangle)]
extern "C" fn acpi_enumerate_devices_ffi(
    cb: AcpiWalkCallback,
    ctx: *mut c_void
) -> ACPI_STATUS {
    unsafe { AcpiGetDevices(core::ptr::null(), Some(cb), ctx, core::ptr::null_mut()) }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_get_hid_ffi(handle: *mut c_void, buf: *mut u8, buf_len: usize) -> usize {
    if handle.is_null() || buf.is_null() || buf_len == 0 {
        return 0;
    }
    unsafe {
        let mut info_ptr: *mut u8 = core::ptr::null_mut();
        let status = AcpiGetObjectInfo(handle, &mut info_ptr);
        if status != AE_OK || info_ptr.is_null() {
            return 0;
        }
        let info = &mut *(info_ptr as *mut AcpiDeviceInfo);
        let result = if info.valid & ACPI_VALID_HID_FLAG != 0 {
            let string_ptr = info.hardware_id.string;
            if !string_ptr.is_null() {
                let hid = core::ffi::CStr::from_ptr(string_ptr).to_bytes();
                let copy_len = hid.len().min(buf_len.saturating_sub(1));
                core::ptr::copy_nonoverlapping(hid.as_ptr(), buf, copy_len);
                *buf.add(copy_len) = 0;
                copy_len
            } else {
                0
            }
        } else {
            0
        };

        AcpiOsFree(info_ptr as *mut c_void);
        result
    }
}

#[unsafe(no_mangle)]
extern "C" fn acpi_get_resources_ffi(
    handle: *mut c_void,
    out: *mut AcpiSimpleResource,
    max: usize,
) -> usize {
    if handle.is_null() || out.is_null() || max == 0 {
        return 0;
    }

    unsafe {
        let mut buf = AcpiBufferRaw {
            length: ACPI_ALLOCATE_BUFFER,
            pointer: core::ptr::null_mut()
        };

        let status = AcpiGetCurrentResources(handle, &mut buf);
        if status != AE_OK || buf.pointer.is_null() {
            return 0;
        }

        let mut count = 0usize;
        let mut ptr = buf.pointer as *const u8;

        loop {
            let header = core::ptr::read_unaligned(ptr as *const AcpiResourceHeader);

            if header.res_type == ACPI_RESOURCE_TYPE_END_TAG || header.length == 0 {
                break;
            }

            let data = ptr.add(core::mem::size_of::<AcpiResourceHeader>());

            if count < max {
                match header.res_type {
                    ACPI_RESOURCE_TYPE_IRQ => {
                        let irq = core::ptr::read_unaligned(data as *const AcpiResourceIrq);

                        let interrupts = data.add(core::mem::offset_of!(AcpiResourceIrq, interrupts))
                            as *const u8;

                        for i in 0..irq.interrupt_count as usize {
                            if count >= max {
                                break;
                            }

                            *out.add(count) = AcpiSimpleResource {
                                res_type: 0,
                                address: *interrupts.add(i) as u64,
                                length: 0,
                            };
                            count += 1;
                        }
                    },
                    ACPI_RESOURCE_TYPE_EXTENDED_IRQ => {
                        let irq = core::ptr::read_unaligned(data as *const AcpiResourceExtendedIrq);

                        let interrupts = data.add(core::mem::offset_of!(
                            AcpiResourceExtendedIrq,
                            interrupts
                        )) as *const u32;

                        for i in 0..irq.interrupt_count as usize {
                            if count >= max {
                                break;
                            }

                            *out.add(count) = AcpiSimpleResource {
                                res_type: 0,
                                address: core::ptr::read_unaligned(interrupts.add(i)) as u64,
                                length: 0,
                            };
                            count += 1;
                        }
                    },
                    ACPI_RESOURCE_TYPE_IO => {
                        let io = core::ptr::read_unaligned(data as *const AcpiResourceIo);

                        *out.add(count) = AcpiSimpleResource {
                            res_type: 1,
                            address: io.minimum as u64,
                            length: io.address_length as u64,
                        };
                        count += 1;
                    },
                    ACPI_RESOURCE_TYPE_FIXED_IO => {
                        let io = core::ptr::read_unaligned(data as *const AcpiResourceFixedIo);

                        *out.add(count) = AcpiSimpleResource {
                            res_type: 1,
                            address: io.address as u64,
                            length: io.address_length as u64,
                        };
                        count += 1;
                    },
                    ACPI_RESOURCE_TYPE_MEMORY32 => {
                        let mem = core::ptr::read_unaligned(data as *const AcpiResourceMemory32);

                        *out.add(count) = AcpiSimpleResource {
                            res_type: 2,
                            address: mem.minimum as u64,
                            length: mem.address_length as u64,
                        };
                        count += 1;
                    },
                    _ => {}
                }
            }

            ptr = ptr.add(header.length as usize);
        }

        AcpiOsFree(buf.pointer);
        count
    }
}